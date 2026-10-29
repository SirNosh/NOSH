#!/usr/bin/env node
// bin.nosh of the nosh-runtime package. The nosh-harness launcher and npm's bin shims run this file with Node;
// it runs the NOSH executable beside it with the same arguments, stdio and exit code.
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const executable = fileURLToPath(new URL("./nosh.exe", import.meta.url));
// Ctrl+C reaches NOSH directly; this process waits for it to finish.
process.on("SIGINT", () => {});
// The Pi sidecar runs on the Node that runs NOSH, as the TypeScript daemon did.
const child = spawn(executable, process.argv.slice(2), { stdio: "inherit", env: { ...process.env, NOSH_NODE: process.env.NOSH_NODE || process.execPath } });
process.exitCode = await new Promise((resolve) => {
  child.once("error", (error) => { process.stderr.write(`nosh: ${error.message}\n`); resolve(1); });
  child.once("exit", (code) => resolve(code ?? 1));
});
