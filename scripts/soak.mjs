import { mkdtempSync, mkdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NoshDaemon } from "../apps/noshd/dist/daemon.js";
import { createId } from "../packages/core/dist/index.js";
import { schemaUri } from "../packages/wire/dist/index.js";

const seconds = Number(process.argv.find((value) => value.startsWith("--seconds="))?.slice(10) ?? "60"); if (!Number.isInteger(seconds) || seconds < 1) throw new Error("--seconds must be a positive integer");
const root = mkdtempSync(join(tmpdir(), "nosh-soak-")); const repository = join(root, "project"); mkdirSync(repository); const projectId = createId("prj"); const databasePath = join(root, "data", "project.sqlite"); const daemon = new NoshDaemon({ dataDirectory: join(root, "data"), bootstrapToken: "soak-test-only" }); const baseline = process.memoryUsage().rss; daemon.start(); daemon.registerProject({ projectId, repositoryRoot: repository, databasePath });
try { for (let second = 0; second < seconds; second += 1) { daemon.appendCommand({ $schema: schemaUri("command"), schemaVersion: 1, commandId: createId("cmd"), idempotencyKey: `soak-event-${String(second).padStart(16, "0")}`, projectId, targetType: "project", targetId: null, expectedVersion: null, type: "event.append", observedVersions: {}, payload: { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "soak.heartbeat", source: "soak", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: null, causationId: null, payload: { second } }, issuedAt: new Date().toISOString() }); await new Promise((resolve) => setTimeout(resolve, 1000)); } const growth = process.memoryUsage().rss - baseline; if (growth > 256 * 1024 * 1024) throw new Error(`RSS grew by ${growth} bytes`); process.stdout.write(`${JSON.stringify({ seconds, events: seconds, rssGrowthBytes: growth, databaseBytes: statSync(databasePath).size })}\n`); }
finally { daemon.stop(); rmSync(root, { recursive: true, force: true }); }
