import { EventEmitter } from "node:events";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { timingSafeEqual } from "node:crypto";
import { EventStore, HostRegistry, type RegisteredProject } from "@nosh/persistence";
import { appendEventCommandSchema, type AppendEventCommand, type EventEnvelope } from "@nosh/wire";
import { SingleInstanceLock } from "./single-instance.js";

export type NoshDaemonOptions = {
  dataDirectory: string;
  bootstrapToken: string;
};

export class NoshDaemon {
  private readonly stores = new Map<string, EventStore>();
  private readonly lock: SingleInstanceLock;
  readonly events = new EventEmitter();
  readonly registry: HostRegistry;

  constructor(private readonly options: NoshDaemonOptions) {
    mkdirSync(options.dataDirectory, { recursive: true });
    this.lock = new SingleInstanceLock(join(options.dataDirectory, "noshd.lock"));
    this.registry = new HostRegistry(join(options.dataDirectory, "host.sqlite"));
  }

  start(): void {
    this.lock.acquire();
  }

  stop(): void {
    for (const store of this.stores.values()) store.close();
    this.stores.clear();
    this.registry.close();
    this.lock.release();
  }

  authenticate(token: string | undefined): boolean {
    if (!token) return false;
    const provided = Buffer.from(token);
    const expected = Buffer.from(this.options.bootstrapToken);
    return provided.length === expected.length && timingSafeEqual(provided, expected);
  }

  registerProject(project: Omit<RegisteredProject, "registeredAt">): RegisteredProject {
    return this.registry.register(project);
  }

  projects(): RegisteredProject[] {
    return this.registry.list();
  }

  appendCommand(command: AppendEventCommand): { event: EventEnvelope; replayed: boolean } {
    const parsed = appendEventCommandSchema.parse(command);
    if (parsed.projectId !== parsed.payload.scope.projectId) throw new Error("Command project scope does not match event project scope");
    const result = this.storeFor(parsed.projectId).appendIdempotent(parsed.idempotencyKey, parsed.payload);
    if (!result.replayed) this.events.emit("event", result.receipt.event);
    return { event: result.receipt.event, replayed: result.replayed };
  }

  replay(projectId: string, afterSequence: number): EventEnvelope[] {
    return this.storeFor(projectId).replay(projectId, afterSequence);
  }

  private storeFor(projectId: string): EventStore {
    const existing = this.stores.get(projectId);
    if (existing) return existing;
    const project = this.projects().find((entry) => entry.projectId === projectId);
    if (!project) throw new Error(`Project ${projectId} is not registered on this host`);
    const store = new EventStore(project.databasePath);
    this.stores.set(projectId, store);
    return store;
  }
}
