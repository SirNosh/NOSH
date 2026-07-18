import { randomBytes } from "node:crypto";
import { resolve } from "node:path";
import { NoshDaemon } from "./daemon.js";
import { LocalApiServer } from "./server.js";

const args = new Map(process.argv.slice(2).map((argument) => {
  const [key, value] = argument.split("=", 2);
  return [key ?? "", value ?? ""];
}));
const dataDirectory = resolve(args.get("--data-dir") || ".noshd");
const port = Number(args.get("--port") || "4321");
const token = args.get("--bootstrap-token") || randomBytes(32).toString("base64url");

const daemon = new NoshDaemon({ dataDirectory, bootstrapToken: token });
daemon.start();
const server = new LocalApiServer(daemon);
await server.listen({ port });

process.on("SIGINT", () => void shutdown());
process.on("SIGTERM", () => void shutdown());

async function shutdown(): Promise<void> {
  await server.close();
  daemon.stop();
  process.exit(0);
}
