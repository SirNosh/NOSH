import { randomBytes, randomUUID } from "node:crypto";
import { DeviceRegistry, RemoteCommandValidator, openRelayPayload, sealRelayPayload, wrapAccountKey, type DeviceKeys, type DeviceRegistryState } from "@nosh/crypto";
import { remoteCommandEnvelopeSchema, schemaUri, type EventEnvelope, type JsonValue } from "@nosh/wire";
import type { NoshDaemon } from "./daemon.js";
import { PairingCoordinator, RelayClient, type RelayFrame } from "./relay-client.js";

export type RemoteConfiguration = { relayUrl: string; channelId: string; adminToken: string; accountId: string; accountKey: string; keyVersion?: number; hostKeys: DeviceKeys; registry: DeviceRegistryState };

export class RemoteControl {
  readonly registry: DeviceRegistry; pairing: PairingCoordinator;
  private accountKey: Uint8Array; private validator: RemoteCommandValidator; private readonly relay: RelayClient; private outbound = Promise.resolve();
  private readonly eventListener = (event: EventEnvelope) => { if (event.type.startsWith("runtime.thread_")) this.queueSnapshot(); if (!remoteSafeEvent(event)) return; this.outbound = this.outbound.then(async () => { const ciphertext = await sealRelayPayload(event as unknown as JsonValue, this.accountKey); this.relay.send({ frameId: frameId(), deviceId: this.configuration.hostKeys.deviceId, kind: "event", ciphertext }); }).catch(() => undefined); };

  constructor(private readonly daemon: NoshDaemon, private readonly configuration: RemoteConfiguration, private readonly save: (configuration: RemoteConfiguration) => Promise<void>) {
    this.accountKey = Buffer.from(configuration.accountKey, "base64url"); this.registry = new DeviceRegistry(configuration.registry);
    const changed = () => { this.configuration.registry = this.registry.snapshot(); void this.save(this.configuration); };
    this.pairing = new PairingCoordinator(this.registry, configuration.relayUrl, configuration.channelId, configuration.adminToken, configuration.accountId, this.accountKey, configuration.keyVersion ?? 1, changed);
    this.validator = new RemoteCommandValidator(this.registry, this.accountKey, configuration.accountId, (command) => daemon.remoteVersion(command), (command) => daemon.remoteReplay(command), configuration.keyVersion ?? 1);
    this.relay = new RelayClient({ relayUrl: configuration.relayUrl, channelId: configuration.channelId, deviceKeys: configuration.hostKeys }, (frame) => void this.receive(frame), (online) => { daemon.events.emit("relay-presence", online); if (online) this.queueSnapshot(); });
  }

  async start(): Promise<void> { await this.pairing.registerRelayDevice(this.configuration.hostKeys); this.daemon.events.on("event", this.eventListener); this.relay.connect(); }
  stop(): void { this.daemon.events.off("event", this.eventListener); this.relay.close(); }
  status(): { relayUrl: string; channelId: string; accountId: string; keyVersion: number; hostDeviceId: string; lastSequence: number; devices: DeviceRegistryState["devices"] } { return { relayUrl: this.configuration.relayUrl, channelId: this.configuration.channelId, accountId: this.configuration.accountId, keyVersion: this.configuration.keyVersion ?? 1, hostDeviceId: this.configuration.hostKeys.deviceId, lastSequence: this.relay.lastSequence(), devices: this.registry.snapshot().devices }; }
  async revoke(deviceId: string): Promise<{ revokedDeviceId: string; rekeyedDeviceIds: string[]; keyVersion: number }> {
    const active = this.registry.snapshot().devices.filter((device) => !device.revokedAt); const revoked = active.find((device) => device.deviceId === deviceId); if (!revoked) throw new Error("Unknown or already revoked device"); const remaining = active.filter((device) => device.deviceId !== deviceId); const oldKey = this.accountKey; const nextKey = randomBytes(32); const keyVersion = (this.configuration.keyVersion ?? 1) + 1;
    await this.pairing.revoke(deviceId, false);
    for (const device of remaining) { const wrappedAccountKey = await wrapAccountKey(nextKey, device.agreementPublicKey); const ciphertext = await sealRelayPayload({ type: "nosh.key-rotation.v1", deviceId: device.deviceId, keyVersion, wrappedAccountKey }, oldKey); await this.pairing.publish({ frameId: frameId(), deviceId: this.configuration.hostKeys.deviceId, kind: "event", ciphertext }); }
    oldKey.fill(0); this.accountKey = nextKey; this.configuration.accountKey = Buffer.from(this.accountKey).toString("base64url"); this.configuration.keyVersion = keyVersion; this.configuration.registry = this.registry.snapshot();
    const changed = () => { this.configuration.registry = this.registry.snapshot(); void this.save(this.configuration); };
    this.pairing = new PairingCoordinator(this.registry, this.configuration.relayUrl, this.configuration.channelId, this.configuration.adminToken, this.configuration.accountId, this.accountKey, this.configuration.keyVersion, changed);
    this.validator = new RemoteCommandValidator(this.registry, this.accountKey, this.configuration.accountId, (command) => this.daemon.remoteVersion(command), (command) => this.daemon.remoteReplay(command), this.configuration.keyVersion);
    await this.save(this.configuration); this.queueSnapshot(); return { revokedDeviceId: deviceId, rekeyedDeviceIds: remaining.map((device) => device.deviceId), keyVersion };
  }

  private async receive(frame: RelayFrame): Promise<void> {
    if (frame.kind !== "command" || !frame.ciphertext) return;
    let response: JsonValue; let commandId: string | undefined;
    try {
      const command = remoteCommandEnvelopeSchema.parse(await openRelayPayload(frame.ciphertext, this.accountKey));
      commandId = command.commandId;
      const validated = await this.validator.validate(command); const applied = validated.replayed && this.daemon.remoteReplay(command) ? { replayed: true, currentVersion: this.daemon.remoteVersion(command) } : await this.daemon.executeRemoteCommand(command, validated.payload);
      response = { commandId: command.commandId, accepted: true, replayed: applied.replayed, currentVersion: applied.currentVersion };
    } catch (error) { response = { ...(commandId ? { commandId } : {}), accepted: false, error: error instanceof Error ? error.message : "remote_command_rejected" }; }
    const ciphertext = await sealRelayPayload(response, this.accountKey); this.relay.send({ frameId: frameId(), deviceId: this.configuration.hostKeys.deviceId, kind: "ack", ciphertext });
  }
  private queueSnapshot(): void {
    this.outbound = this.outbound.then(async () => {
      const registered = this.daemon.projects();
      const projects = registered.map((project) => ({ projectId: project.projectId, repositoryRoot: project.repositoryRoot, registeredAt: project.registeredAt }));
      const common = {
        type: "nosh.snapshot.v1", generatedAt: new Date().toISOString(), projects,
        missions: Object.fromEntries(registered.map((project) => [project.projectId, this.daemon.research.missions(project.projectId)])),
        directions: Object.fromEntries(registered.map((project) => [project.projectId, this.daemon.research.directions(project.projectId)])),
        autoresearch: Object.fromEntries(registered.map((project) => [project.projectId, this.daemon.research.autoresearch(project.projectId)])),
        threads: Object.fromEntries(registered.map((project) => [project.projectId, this.daemon.runtime.threads(project.projectId).filter(({ value }) => value.executionMode === "foreground_fork" && ["open", "running", "awaiting", "awaiting_user", "paused"].includes(value.state)).slice(-20)])),
        agents: this.daemon.agents.inspect(), jobs: this.daemon.jobs.list(),
      };
      let snapshot = {
        ...common,
        records: Object.fromEntries(registered.map((project) => [project.projectId, this.daemon.research.records(project.projectId).filter(remoteSafeRecord).slice(-100)])),
        papers: Object.fromEntries(registered.map((project) => { const paper = this.daemon.research.readPaper(project.projectId); return [project.projectId, { ...paper, markdown: paper.markdown.slice(0, 100_000) }]; })),
        events: Object.fromEntries(registered.map((project) => [project.projectId, this.daemon.replay(project.projectId, 0).filter(remoteSafeEvent).slice(-100)])),
      } as unknown as JsonValue;
      if (JSON.stringify(snapshot).length > 650_000) snapshot = { ...common, records: {}, papers: {}, events: Object.fromEntries(registered.map((project) => [project.projectId, this.daemon.replay(project.projectId, 0).filter(remoteSafeEvent).slice(-25)])) } as unknown as JsonValue;
      const ciphertext = await sealRelayPayload(snapshot, this.accountKey);
      this.relay.send({ frameId: frameId(), deviceId: this.configuration.hostKeys.deviceId, kind: "snapshot", ciphertext });
    }).catch(() => undefined);
  }
}

function frameId(): string { return `frm_${randomUUID().replaceAll("-", "")}`; }
function remoteSafeRecord(record: JsonValue): boolean { return !record || typeof record !== "object" || Array.isArray(record) || ![schemaUri("episode-draft"), schemaUri("runtime-instruction")].includes(String(record.$schema)); }
function remoteSafeEvent(event: EventEnvelope): boolean { return !event.type.startsWith("runtime.") && !(event.type === "record.submitted" && !remoteSafeRecord(event.payload)); }
