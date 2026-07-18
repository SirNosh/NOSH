import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { Duplex } from "node:stream";
import { URL } from "node:url";
import { WebSocketServer, type WebSocket } from "ws";
import { appendEventCommandSchema } from "@nosh/wire";
import { NoshDaemon } from "./daemon.js";

type ServerOptions = { host?: "127.0.0.1" | "::1"; port: number };

export class LocalApiServer {
  private readonly http: Server;
  private readonly websocket = new WebSocketServer({ noServer: true });

  constructor(private readonly daemon: NoshDaemon) {
    this.http = createServer((request, response) => void this.handle(request, response));
    this.http.on("upgrade", (request, socket, head) => this.handleUpgrade(request, socket, head));
  }

  async listen(options: ServerOptions): Promise<void> {
    await new Promise<void>((resolve, reject) => {
      this.http.once("error", reject);
      this.http.listen(options.port, options.host ?? "127.0.0.1", () => {
        this.http.off("error", reject);
        resolve();
      });
    });
  }

  async close(): Promise<void> {
    this.websocket.clients.forEach((client) => client.close());
    await new Promise<void>((resolve, reject) => this.http.close((error) => (error ? reject(error) : resolve())));
  }

  address(): string {
    const address = this.http.address();
    if (!address || typeof address === "string") throw new Error("Server is not listening");
    return `http://${address.address.includes(":") ? `[${address.address}]` : address.address}:${address.port}`;
  }

  private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? "/", "http://localhost");
    if (request.method === "GET" && url.pathname === "/health") {
      this.send(response, 200, { status: "ok" });
      return;
    }
    if (!this.authorized(request)) {
      this.send(response, 401, { error: "unauthorized" });
      return;
    }

    try {
      if (request.method === "GET" && url.pathname === "/projects") {
        this.send(response, 200, { projects: this.daemon.projects() });
        return;
      }
      if (request.method === "POST" && url.pathname === "/projects") {
        const project = await readJson(request);
        this.send(response, 201, { project: this.daemon.registerProject(project as { projectId: string; repositoryRoot: string; databasePath: string }) });
        return;
      }
      if (request.method === "GET" && url.pathname === "/events") {
        const projectId = url.searchParams.get("projectId");
        const after = Number(url.searchParams.get("after") ?? "0");
        if (!projectId || !Number.isSafeInteger(after) || after < 0) throw new Error("projectId and a non-negative after cursor are required");
        this.send(response, 200, { events: this.daemon.replay(projectId, after) });
        return;
      }
      if (request.method === "POST" && url.pathname === "/commands") {
        const command = appendEventCommandSchema.parse(await readJson(request));
        this.send(response, 202, this.daemon.appendCommand(command));
        return;
      }
      this.send(response, 404, { error: "not_found" });
    } catch (error) {
      this.send(response, 400, { error: error instanceof Error ? error.message : "invalid_request" });
    }
  }

  private handleUpgrade(request: IncomingMessage, socket: Duplex, head: Buffer): void {
    const url = new URL(request.url ?? "/", "http://localhost");
    const projectId = url.searchParams.get("projectId");
    const after = Number(url.searchParams.get("after") ?? "0");
    if (url.pathname !== "/events" || !projectId || !Number.isSafeInteger(after) || after < 0 || !this.authorized(request)) {
      socket.destroy();
      return;
    }

    this.websocket.handleUpgrade(request, socket, head, (client) => this.subscribe(client, projectId, after));
  }

  private subscribe(client: WebSocket, projectId: string, after: number): void {
    let lastSequence = after;
    for (const event of this.daemon.replay(projectId, after)) {
      client.send(JSON.stringify(event));
      lastSequence = event.sequence ?? lastSequence;
    }
    const listener = (event: { scope: { projectId: string }; sequence: number | null }) => {
      if (event.scope.projectId !== projectId || (event.sequence !== null && event.sequence <= lastSequence)) return;
      if (event.sequence !== null) lastSequence = event.sequence;
      if (client.readyState === client.OPEN) client.send(JSON.stringify(event));
    };
    this.daemon.events.on("event", listener);
    client.once("close", () => this.daemon.events.off("event", listener));
  }

  private authorized(request: IncomingMessage): boolean {
    const match = /^Bearer (.+)$/.exec(request.headers.authorization ?? "");
    return this.daemon.authenticate(match?.[1]);
  }

  private send(response: ServerResponse, status: number, body: unknown): void {
    response.writeHead(status, { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" });
    response.end(JSON.stringify(body));
  }
}

async function readJson(request: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of request) {
    const bytes = Buffer.from(chunk);
    size += bytes.length;
    if (size > 1_000_000) throw new Error("request body exceeds 1 MB");
    chunks.push(bytes);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}
