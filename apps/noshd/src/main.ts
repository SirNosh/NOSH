import { randomBytes } from "node:crypto";
import { existsSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { decryptVault, encryptVault, type Vault } from "@nosh/crypto";
import { NoshDaemon } from "./daemon.js";
import { RemoteControl, type RemoteConfiguration } from "./remote-control.js";
import { LocalApiServer } from "./server.js";

const args = new Map(process.argv.slice(2).map((argument) => {
  const [key, value] = argument.split("=", 2);
  return [key ?? "", value ?? ""];
}));
const dataDirectory = resolve(args.get("--data-dir") || ".noshd");
const port = Number(args.get("--port") || "4321");
const token = args.get("--bootstrap-token") || process.env.NOSH_BOOTSTRAP_TOKEN || randomBytes(32).toString("base64url");
const webDirectory = resolve(args.get("--web-dir") || fileURLToPath(new URL("../../web/dist", import.meta.url)));
const remoteVaultPath = args.get("--remote-vault") ? resolve(args.get("--remote-vault")!) : null;

const daemon = new NoshDaemon({ dataDirectory, bootstrapToken: token });
daemon.start();
const remote = remoteVaultPath && existsSync(remoteVaultPath) ? new RemoteControl(daemon, await loadRemote(remoteVaultPath, token), (configuration) => saveRemote(remoteVaultPath, token, configuration)) : undefined;
const server = new LocalApiServer(daemon, webDirectory, remote);
await server.listen({ port });
await remote?.start();

process.on("SIGINT", () => void shutdown());
process.on("SIGTERM", () => void shutdown());
daemon.events.once("shutdown-request", () => void shutdown());

async function shutdown(): Promise<void> {
  remote?.stop();
  await server.close();
  daemon.stop();
  process.exit(0);
}

async function loadRemote(path: string, password: string): Promise<RemoteConfiguration> { return await decryptVault(JSON.parse(readFileSync(path, "utf8")) as Vault, password) as unknown as RemoteConfiguration; }
async function saveRemote(path: string, password: string, configuration: RemoteConfiguration): Promise<void> { const temporary = `${path}.tmp`; writeFileSync(temporary, `${JSON.stringify(await encryptVault(configuration as unknown as import("@nosh/wire").JsonValue, password))}\n`, { encoding: "utf8", mode: 0o600 }); renameSync(temporary, path); }
