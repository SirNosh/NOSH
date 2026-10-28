import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { RELEASES_URL, compareVersions, latestRelease, maybeNotify, readCurrent, runNpm, update, verifySha256 } from "./lib.mjs";

const work = mkdtempSync(join(tmpdir(), "nosh-launcher-"));
const packed = {};

// A stand-in harness that honours the launcher contract: `help`, `stop --if-idle` exit codes, `start`.
function packRuntime(version) {
  const source = join(work, `runtime-${version}`); mkdirSync(source, { recursive: true });
  writeFileSync(join(source, "package.json"), JSON.stringify({ name: "nosh-runtime", version, type: "module", bin: { nosh: "cli.mjs" } }));
  writeFileSync(join(source, "cli.mjs"), `import { writeFileSync } from "node:fs";
const [command] = process.argv.slice(2);
if (command === "help") console.log("nosh setup");
else if (command === "stop") process.exit(Number(process.env.FAKE_STOP_CODE ?? 3));
else if (command === "start") writeFileSync(process.env.FAKE_START_MARKER, "${version}");
else { console.log(JSON.stringify(process.argv.slice(2))); process.exit(7); }
`);
  runNpm(["pack", source, "--pack-destination", work], work);
  packed[version] = readFileSync(join(work, `nosh-runtime-${version}.tgz`));
}

/** GitHub as seen by the launcher: the releases API plus two downloadable assets. */
function github(version, { checksum } = {}) {
  const name = `nosh-runtime-${version}.tgz`;
  const files = { [`https://example.test/${name}`]: packed[version], [`https://example.test/${name}.sha256`]: Buffer.from(`${checksum ?? createHash("sha256").update(packed[version]).digest("hex")}  ${name}\n`) };
  return async (url) => url === RELEASES_URL
    ? new Response(JSON.stringify({ tag_name: `v${version}`, assets: [{ name, browser_download_url: `https://example.test/${name}` }, { name: `${name}.sha256`, browser_download_url: `https://example.test/${name}.sha256` }] }))
    : new Response(files[url], { status: files[url] ? 200 : 404 });
}

beforeAll(() => { for (const version of ["0.0.1", "0.0.2", "0.0.3"]) packRuntime(version); }, 120_000);
afterAll(() => rmSync(work, { recursive: true, force: true }));

describe("launcher helpers", () => {
  it("orders versions numerically", () => {
    expect(compareVersions("0.10.0", "0.9.9")).toBe(1);
    expect(compareVersions("1.0.0", "1.0.0")).toBe(0);
    expect(compareVersions("0.1.1", "0.2.0")).toBe(-1);
  });
  it("rejects a release without a runtime package or checksum, and a mismatched checksum", async () => {
    const answer = (assets) => async () => new Response(JSON.stringify({ tag_name: "v9.9.9", assets }));
    await expect(latestRelease(answer([]))).rejects.toThrow("no nosh-runtime package");
    await expect(latestRelease(answer([{ name: "nosh-runtime-9.9.9.tgz", browser_download_url: "x" }]))).rejects.toThrow("no checksum");
    expect(() => verifySha256(Buffer.from("a"), `${"0".repeat(64)}  file`)).toThrow("Checksum mismatch");
  });
});

describe("nosh update", () => {
  it("installs, waits for active work, switches with a restart, and refuses a corrupted download", async () => {
    const root = join(work, "state", "harness"); const marker = join(work, "started"); const log = () => {};
    process.env.FAKE_START_MARKER = marker;
    try {
      expect(await update(root, { fetchImpl: github("0.0.1"), log })).toBe(0);
      expect(readCurrent(root).version).toBe("0.0.1");

      process.env.FAKE_STOP_CODE = "2";
      expect(await update(root, { fetchImpl: github("0.0.2"), log })).toBe(2);
      expect(readCurrent(root).version).toBe("0.0.1");
      expect(existsSync(join(root, "versions", "0.0.2", ".complete"))).toBe(true);

      process.env.FAKE_STOP_CODE = "0";
      expect(await update(root, { fetchImpl: github("0.0.2"), log })).toBe(0);
      expect(readCurrent(root).version).toBe("0.0.2");
      expect(readFileSync(marker, "utf8")).toBe("0.0.2");
      expect(await update(root, { fetchImpl: github("0.0.2"), log })).toBe(0);

      await expect(update(root, { fetchImpl: github("0.0.3", { checksum: "f".repeat(64) }), log })).rejects.toThrow("Checksum mismatch");
      expect(readCurrent(root).version).toBe("0.0.2");
      expect(existsSync(join(root, "versions", "0.0.3"))).toBe(false);
      expect(existsSync(join(root, "update.lock"))).toBe(false);
    } finally { delete process.env.FAKE_STOP_CODE; delete process.env.FAKE_START_MARKER; }
  }, 180_000);

  it("forwards arguments and the exit code, and checks for updates at most once a day", async () => {
    const state = join(work, "forward"); const root = join(state, "NOSH", "harness");
    await update(root, { fetchImpl: github("0.0.1"), log: () => {} });
    const run = spawnSync(process.execPath, [join(import.meta.dirname, "nosh.mjs"), "mission", "list", "--x=1"], { encoding: "utf8", env: { ...process.env, LOCALAPPDATA: state, APPDATA: state } });
    expect(run.status).toBe(7);
    expect(run.stdout).toContain(JSON.stringify(["mission", "list", "--x=1"]));

    const notices = []; let calls = 0;
    const counting = async (url, options) => { calls += 1; return github("0.0.2")(url, options); };
    await maybeNotify(root, { fetchImpl: counting, now: Date.now(), log: (line) => notices.push(line) });
    expect(calls).toBe(0);
    await maybeNotify(root, { fetchImpl: counting, now: Date.now() + 25 * 60 * 60 * 1000, log: (line) => notices.push(line) });
    expect(calls).toBe(1);
    expect(notices).toEqual(["NOSH 0.0.2 is available (you have 0.0.1). Run `nosh update`."]);
  }, 120_000);
});
