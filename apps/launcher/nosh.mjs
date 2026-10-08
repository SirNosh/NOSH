#!/usr/bin/env node
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { compareVersions, harnessRoot, latestRelease, maybeNotify, readCurrent, update } from "./lib.mjs";

const root = harnessRoot();
const args = process.argv.slice(2);

try {
  if (args[0] === "update" && args.includes("--check")) {
    const current = readCurrent(root); const latest = await latestRelease();
    process.stdout.write(`installed ${current?.version ?? "none"}, latest ${latest.version}${current && compareVersions(latest.version, current.version) <= 0 ? " (up to date)" : "; run `nosh update`"}\n`);
  } else if (args[0] === "update") {
    process.exitCode = await update(root);
  } else if (args[0] === "--version") {
    const { version } = JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8"));
    process.stdout.write(`NOSH ${readCurrent(root)?.version ?? "not installed"} (nosh-harness launcher ${version})\n`);
  } else {
    if (!readCurrent(root)) {
      process.stdout.write("First run: downloading NOSH from GitHub releases.\n");
      if (await update(root) !== 0) throw new Error("NOSH could not be installed");
    }
    // Ctrl+C reaches the harness directly; the launcher waits for it to finish.
    process.on("SIGINT", () => {});
    const child = spawn(process.execPath, [readCurrent(root).entry, ...args], { stdio: "inherit" });
    const code = await new Promise((resolve) => child.on("exit", (exitCode) => resolve(exitCode ?? 1)));
    await maybeNotify(root);
    process.exitCode = code;
  }
} catch (error) {
  process.stderr.write(`nosh: ${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
}
