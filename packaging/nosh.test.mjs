// Tests of the bin.nosh stub (packaging/nosh.mjs). The stand-in "NOSH" is a copy of this Node (or of cmd.exe),
// so the stub runs unmodified and needs no test hooks. Run with `node --test packaging/nosh.test.mjs`.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import { fileURLToPath } from "node:url";

const ARGS = ["a b", 'q"uote', "", "ünï ✓", "--x=1=2", "tail\\", 'back\\\\"slash', "%PATH%", "^&|<>"];
const work = mkdtempSync(join(tmpdir(), "nosh-stub-"));
after(() => rmSync(work, { recursive: true, force: true }));

/** `<work>/<name>/bin/nosh.mjs` (the real stub) beside a copy of `executable` named nosh.exe; returns the stub. */
function stub(name, executable) {
  const bin = join(work, name, "bin");
  mkdirSync(bin, { recursive: true });
  copyFileSync(fileURLToPath(new URL("./nosh.mjs", import.meta.url)), join(bin, "nosh.mjs"));
  if (executable) copyFileSync(executable, join(bin, "nosh.exe"));
  return join(bin, "nosh.mjs");
}

const node = stub("node", process.execPath);
// The stand-in never reads NOSH state, but no process this suite starts may see the real one.
const run = (file, args, options = {}) => spawnSync(process.execPath, [file, ...args], { encoding: "utf8", ...options, env: { ...process.env, LOCALAPPDATA: work, APPDATA: work, ...options.env } });

test("passes arguments through unchanged", () => {
  const result = run(node, ["-e", "process.stdout.write(JSON.stringify(process.argv.slice(1)))", "--", ...ARGS]);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout), ARGS);
});

test("passes the exit code through", () => {
  for (const code of [0, 1, 2, 3, 7, 255, ...(process.platform === "win32" ? [3221225786, 3221225477] : [])]) {
    assert.equal(run(node, ["-e", `process.exit(${code})`]).status, code);
  }
});

test("passes stdin, stdout and stderr through", () => {
  const result = run(node, ["-e", "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{process.stdout.write(d.toUpperCase());process.stderr.write('err\\n')})"], { input: "line one\nline two\n" });
  assert.equal(result.stdout, "LINE ONE\nLINE TWO\n");
  assert.equal(result.stderr, "err\n");
  assert.equal(result.status, 0);
});

test("reports a missing executable", () => {
  const result = run(stub("missing"), ["help"]);
  assert.equal(result.status, 1);
  assert.ok(result.stderr.startsWith("nosh: spawn "), result.stderr);
  assert.ok(result.stderr.endsWith("nosh.exe ENOENT\n"), result.stderr);
});

test("hands NOSH this Node unless NOSH_NODE is set", () => {
  assert.equal(run(node, ["-e", "process.stdout.write(process.env.NOSH_NODE)"], { env: { NOSH_NODE: "" } }).stdout, process.execPath);
  assert.equal(run(node, ["-e", "process.stdout.write(process.env.NOSH_NODE)"], { env: { NOSH_NODE: "C:\\custom\\node.exe" } }).stdout, "C:\\custom\\node.exe");
});

test("background processes outlive the stub (win32 only)", { skip: process.platform !== "win32" }, async () => {
  const marker = join(work, "survived.txt");
  const script = `setTimeout(() => require("fs").writeFileSync(${JSON.stringify(marker)}, "survived"), 2000)`;
  const result = run(stub("cmd", join(process.env.SystemRoot ?? "C:\\Windows", "System32", "cmd.exe")), ["/d", "/c", "start", '""', "/b", process.execPath, "-e", script], { stdio: "ignore" });
  assert.equal(result.status, 0);
  assert.ok(!existsSync(marker), "the stub waited for the background process");
  for (let waited = 0; waited < 10_000 && !existsSync(marker); waited += 100) await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal(readFileSync(marker, "utf8"), "survived");
});
