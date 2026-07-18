import WebSocket from "ws";
import { DeviceRegistry, signRelayChallenge, type DeviceKeys, type PairingCapability } from "@nosh/crypto";

export type RelayFrame = { sequence?: number; frameId: string; deviceId: string; kind: "event" | "snapshot" | "command" | "ack"; ciphertext?: string };
export type RelayClientOptions = { relayUrl: string; channelId: string; deviceKeys: DeviceKeys; initialSequence?: number; reconnectMaximumMs?: number };

export class RelayClient {
  private socket: WebSocket | undefined; private stopped = false; private reconnectMs = 1_000; private cursor: number; private timer: NodeJS.Timeout | undefined;
  constructor(private readonly options: RelayClientOptions, private readonly receive: (frame: RelayFrame) => void, private readonly presence: (online: boolean) => void = () => undefined) { this.cursor = options.initialSequence ?? 0; }
  connect(): void { this.stopped = false; void this.connectAuthenticated().catch(() => { this.presence(false); if (!this.stopped) this.reconnect(); }); }
  private async connectAuthenticated(): Promise<void> {
    const challengeUrl = new URL(`/v1/channels/${this.options.channelId}/challenge`, this.options.relayUrl); const response = await fetch(challengeUrl, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ deviceId: this.options.deviceKeys.deviceId }) }); if (!response.ok) throw new Error(`Relay challenge failed (${response.status})`);
    const { challenge } = await response.json() as { challenge: string }; const signature = await signRelayChallenge(this.options.channelId, challenge, this.options.deviceKeys);
    const url = new URL(`/v1/channels/${this.options.channelId}`, this.options.relayUrl); url.protocol = url.protocol === "https:" ? "wss:" : "ws:"; url.searchParams.set("deviceId", this.options.deviceKeys.deviceId); url.searchParams.set("after", String(this.cursor)); url.searchParams.set("challenge", challenge); url.searchParams.set("signature", signature);
    const socket = new WebSocket(url); this.socket = socket;
    socket.on("open", () => { this.reconnectMs = 1_000; this.presence(true); });
    socket.on("message", (bytes) => { const frame = JSON.parse(bytes.toString()) as RelayFrame; if (frame.sequence && frame.sequence > this.cursor) this.cursor = frame.sequence; this.receive(frame); });
    socket.on("close", () => { this.presence(false); if (!this.stopped) this.reconnect(); });
    socket.on("error", () => socket.close());
  }
  send(frame: Omit<RelayFrame, "sequence"> & { ciphertext: string }): boolean { if (this.socket?.readyState !== WebSocket.OPEN) return false; this.socket.send(JSON.stringify(frame)); return true; }
  lastSequence(): number { return this.cursor; }
  close(): void { this.stopped = true; if (this.timer) clearTimeout(this.timer); this.socket?.close(); }
  private reconnect(): void { this.timer = setTimeout(() => this.connect(), this.reconnectMs); this.timer.unref(); this.reconnectMs = Math.min(this.reconnectMs * 2, this.options.reconnectMaximumMs ?? 30_000); }
}

export class PairingCoordinator {
  constructor(private readonly registry: DeviceRegistry, private readonly relayUrl: string, private readonly channelId: string, private readonly adminToken: string, private readonly accountId: string, private readonly accountKey: Uint8Array, private readonly keyVersion = 1, private readonly changed: () => void = () => undefined) {}
  async create(): Promise<PairingCapability> { const pairing = await this.registry.createPairing(); await this.request("/pairings", { method: "POST", body: JSON.stringify({ capability: pairing.capability, expiresAt: pairing.expiresAt }) }); this.changed(); return pairing; }
  async registerRelayDevice(device: { deviceId: string; signingPublicKey: string; agreementPublicKey: string }): Promise<void> { await this.request("/devices", { method: "POST", body: JSON.stringify(device) }); }
  async candidate(capability: string): Promise<{ deviceId: string; signingPublicKey: string; agreementPublicKey: string } | null> { const response = await this.request(`/pairings?capability=${encodeURIComponent(capability)}`); return (await response.json() as { candidate: { deviceId: string; signingPublicKey: string; agreementPublicKey: string } | null }).candidate; }
  async approve(capability: string, verificationCode: string, permissions: string[]): Promise<{ deviceId: string; keyVersion: number }> { const candidate = await this.candidate(capability); if (!candidate) throw new Error("Remote device has not presented its public keys"); this.registry.present(capability, candidate); const approved = this.registry.approve(capability, verificationCode, permissions, this.accountKey); await this.request("/pairings/approve", { method: "POST", body: JSON.stringify({ capability, accountId: this.accountId, keyVersion: this.keyVersion, deviceId: approved.device.deviceId, signingPublicKey: approved.device.signingPublicKey, agreementPublicKey: approved.device.agreementPublicKey, wrappedAccountKey: approved.wrappedAccountKey }) }); this.changed(); return { deviceId: approved.device.deviceId, keyVersion: this.keyVersion }; }
  async revoke(deviceId: string, notify = true): Promise<void> { this.registry.revoke(deviceId); await this.request("/revoke", { method: "POST", body: JSON.stringify({ deviceId }) }); if (notify) this.changed(); }
  async publish(frame: { frameId: string; deviceId: string; kind: "event" | "snapshot" | "command" | "ack"; ciphertext: string }): Promise<void> { await this.request("/frames", { method: "POST", body: JSON.stringify(frame) }); }
  private request(path: string, init: RequestInit = {}): Promise<Response> { const url = new URL(`/v1/channels/${this.channelId}${path}`, this.relayUrl); return fetch(url, { ...init, headers: { authorization: `Bearer ${this.adminToken}`, "content-type": "application/json", ...init.headers } }).then((response) => { if (!response.ok) throw new Error(`Relay request failed (${response.status})`); return response; }); }
}
