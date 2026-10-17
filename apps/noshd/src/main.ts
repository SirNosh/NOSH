import { randomBytes } from "node:crypto";
import { resolve } from "node:path";
import { NoshDaemon } from "./daemon.js";
import { LocalApiServer } from "./server.js";
import { parseModelSelection } from "@nosh/wire";

const args = new Map(process.argv.slice(2).map((argument) => {
  const [key, value] = argument.split("=", 2);
  return [key ?? "", value ?? ""];
}));
const dataDirectory = resolve(args.get("--data-dir") || ".noshd");
const port = Number(args.get("--port") || "4321");
const token = args.get("--bootstrap-token") || process.env.NOSH_BOOTSTRAP_TOKEN || randomBytes(32).toString("base64url");

const defaultModel = process.env.NOSH_DEFAULT_MODEL ? parseModelSelection(process.env.NOSH_DEFAULT_MODEL) : undefined;
const daemon = new NoshDaemon({ dataDirectory, bootstrapToken: token, ...(defaultModel ? { defaultModel } : {}) });
// stdout/stderr are the daemon log (`nosh logs`): lifecycle and failure events only, never model text or record bodies.
const operatorEvent = /(error|failed|blocked|halted|exhausted|paused|stopped|closed|completed|accepted|resolved|recovery\.report)$/;
daemon.events.on("event", (event: { type: string; source?: string; scope?: Record<string, unknown>; payload?: unknown }) => {
  if (!operatorEvent.test(event.type) || /^(agent|runtime|record)\./.test(event.type)) return;
  const scope = Object.entries(event.scope ?? {}).filter(([key, value]) => key !== "projectId" && typeof value === "string").map(([key, value]) => `${key}=${String(value)}`).join(" ");
  const message = (event.payload as { message?: unknown } | undefined)?.message;
  console.log(`${new Date().toISOString()} ${event.type} ${scope}${typeof message === "string" ? ` :: ${message.slice(0, 300)}` : ""}`);
});
daemon.start();
const server = new LocalApiServer(daemon);
await server.listen({ port });
console.log(`${new Date().toISOString()} noshd listening on 127.0.0.1:${port} (data ${dataDirectory})`);

process.on("SIGINT", () => void shutdown());
process.on("SIGTERM", () => void shutdown());
daemon.events.once("shutdown-request", () => void shutdown());

async function shutdown(): Promise<void> {
  console.log(`${new Date().toISOString()} noshd shutting down`);
  await server.close();
  daemon.stop();
  process.exit(0);
}
