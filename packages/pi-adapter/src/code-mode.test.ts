import { describe, expect, it } from "vitest";
// The worker is compiled JavaScript loaded next to code-mode.js, so the sandbox is exercised from dist (built by `pnpm test`).
import type * as CodeMode from "./code-mode.js";
const { CODE_LIMITS, runProgram } = await import(new URL("../dist/code-mode.js", import.meta.url).href) as typeof CodeMode;
type ProgramHost = CodeMode.ProgramHost;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
const never = new AbortController().signal;
const echo: ProgramHost = async (name, args) => { await sleep(200); return { name, args, at: Date.now() }; };

describe("nosh_code sandbox", () => {
  it("reaches no Node or host globals beyond the tool functions", async () => {
    const result = await runProgram(`return [typeof require, typeof process, typeof fetch, typeof setTimeout, typeof globalThis.__call, typeof globalThis.__log, Object.keys(tools),
      (() => { try { return typeof (function () {}).constructor("return process")(); } catch { return "threw"; } })(),
      (() => { try { return typeof (async () => {}).constructor("return import('node:fs')")(); } catch { return "threw"; } })()];`, ["read"], echo, never);
    // Reaching for process through the Function constructor throws: the name does not exist in the guest.
    expect(result).toMatchObject({ ok: true, value: ["undefined", "undefined", "undefined", "undefined", "undefined", "undefined", ["read"], "threw", "object"] });
  });

  it("refuses dynamic import of any module", async () => {
    const result = await runProgram(`try { await import("node:fs"); return "imported"; } catch (error) { return "refused"; }`, [], echo, never);
    expect(result.value).toBe("refused");
  });

  it("runs independent tool calls concurrently and chains dependent ones", async () => {
    // Each host call takes 200 ms; the two parallel calls finish within 100 ms of each other only if they ran together.
    const result = await runProgram(`const [a, b] = await Promise.all([tools.read("a.txt"), tools.read("b.txt")]); const c = await tools.read(a.args[0] + "+" + b.args[0]); return [c.args[0], Math.abs(a.at - b.at) < 100];`, ["read"], echo, never);
    expect(result).toMatchObject({ ok: true, value: ["a.txt+b.txt", true], calls: 3 });
  });

  it("returns a failed tool call to the program as an Error it can catch", async () => {
    const host: ProgramHost = async () => { throw new Error("Workspace path escapes the assigned root"); };
    const result = await runProgram(`try { await tools.read("../x"); } catch (error) { return error instanceof Error ? error.message : "not an Error"; }`, ["read"], host, never);
    expect(result).toMatchObject({ ok: true, value: "Workspace path escapes the assigned root" });
  });

  it("stops a busy loop at the CPU limit without stalling the main event loop", async () => {
    let ticks = 0; const timer = setInterval(() => ticks += 1, 20);
    const result = await runProgram(`while (true) {}`, [], echo, never, { ...CODE_LIMITS, cpuMs: 400 });
    clearInterval(timer);
    expect(result.ok).toBe(false);
    expect(result.error).toMatch(/interrupted/i);
    expect(ticks).toBeGreaterThanOrEqual(10);
  });

  it("does not count time spent waiting on tools against the CPU limit", async () => {
    const result = await runProgram(`for (let i = 0; i < 4; i++) await tools.read(String(i)); return "done";`, ["read"], echo, never, { ...CODE_LIMITS, cpuMs: 300 });
    expect(result).toMatchObject({ ok: true, value: "done", calls: 4 });
  });

  it("enforces the memory limit", async () => {
    const result = await runProgram(`const chunks = []; while (true) chunks.push(new ArrayBuffer(4 << 20));`, [], echo, never, { ...CODE_LIMITS, memoryBytes: 16 * 1024 * 1024 });
    expect(result.ok).toBe(false);
    expect(result.error).toMatch(/memory/i);
  });

  it("refuses tool calls beyond the per-program cap", async () => {
    const result = await runProgram(`const outcomes = []; for (let i = 0; i < 4; i++) { try { await tools.read(String(i)); outcomes.push("ok"); } catch (error) { outcomes.push(error.message); } } return outcomes;`, ["read"], async () => "x", never, { ...CODE_LIMITS, maxCalls: 3 });
    expect(result).toMatchObject({ ok: true, value: ["ok", "ok", "ok", "Program exceeded 3 tool calls"], calls: 3 });
  });

  it("reports thrown errors, syntax errors, and programs that can never finish", async () => {
    expect(await runProgram(`throw new TypeError("bad input");`, [], echo, never)).toMatchObject({ ok: false, error: "TypeError: bad input" });
    expect((await runProgram(`return (;`, [], echo, never)).error).toMatch(/SyntaxError/);
    expect((await runProgram(`await new Promise(() => {});`, [], echo, never)).error).toMatch(/never finish/);
  });

  it("captures console.log output within the log budget and returns null for no value", async () => {
    const result = await runProgram(`console.log("files:", [1, 2]); console.log("x".repeat(50));`, [], echo, never, { ...CODE_LIMITS, maxLogChars: 30 });
    expect(result.ok).toBe(true);
    expect(result.value).toBeNull();
    expect(result.logs.join("")).toHaveLength(30);
    expect(result.logs[0]).toBe("files: [1,2]");
  });

  it("stops at once when cancelled, even while spinning", async () => {
    const controller = new AbortController(); setTimeout(() => controller.abort(), 150);
    const started = performance.now();
    const result = await runProgram(`while (true) {}`, [], echo, controller.signal, { ...CODE_LIMITS, cpuMs: 60_000 });
    expect(result).toMatchObject({ ok: false, error: "Program cancelled" });
    expect(performance.now() - started).toBeLessThan(1_000);
    controller.abort();
    expect(await runProgram(`return 1;`, [], echo, controller.signal)).toMatchObject({ ok: false, error: "Program cancelled" });
  });

  it("rejects programs over the size limit before starting a worker", async () => {
    expect(await runProgram(" ".repeat(20_001), [], echo, never)).toMatchObject({ ok: false, error: "Program exceeds 20000 characters" });
  });
});
