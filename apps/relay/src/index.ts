import { DurableObject } from "cloudflare:workers";
import { parseRelayFrame, type RelayFrame } from "./frame.js";

export interface Env {
  CHANNELS: DurableObjectNamespace<Channel>;
  RELAY_ADMIN_TOKEN: string;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/health") return Response.json({ status: "ok", plaintextAuthority: false });
    const match = /^\/v1\/channels\/([a-z0-9_-]{16,128})(?:\/.*)?$/.exec(url.pathname);
    if (!match?.[1]) return Response.json({ error: "not_found" }, { status: 404 });
    const id = env.CHANNELS.idFromName(match[1]);
    return env.CHANNELS.get(id).fetch(request);
  },
} satisfies ExportedHandler<Env>;

export class Channel extends DurableObject<Env> {
  constructor(state: DurableObjectState, env: Env) {
    super(state, env);
    this.ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS frames (sequence INTEGER PRIMARY KEY AUTOINCREMENT, frame_id TEXT NOT NULL UNIQUE, device_id TEXT NOT NULL, kind TEXT NOT NULL, ciphertext TEXT NOT NULL, created_at INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS devices (device_id TEXT PRIMARY KEY, signing_public_key TEXT NOT NULL, agreement_public_key TEXT NOT NULL, revoked_at INTEGER);
      CREATE TABLE IF NOT EXISTS pairings (capability TEXT PRIMARY KEY, expires_at INTEGER NOT NULL, candidate_json TEXT, wrapped_account_key TEXT, account_id TEXT, key_version INTEGER, approved_device_id TEXT, used_at INTEGER);
      CREATE TABLE IF NOT EXISTS challenges (challenge TEXT PRIMARY KEY, device_id TEXT NOT NULL, expires_at INTEGER NOT NULL, used_at INTEGER);
    `);
    try { this.ctx.storage.sql.exec("ALTER TABLE pairings ADD COLUMN account_id TEXT"); } catch { /* already migrated */ }
    try { this.ctx.storage.sql.exec("ALTER TABLE pairings ADD COLUMN key_version INTEGER"); } catch { /* already migrated */ }
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/pairings") && request.method === "POST") {
      const body = await request.json<{ capability: string; expiresAt: string; deviceId?: string; signingPublicKey?: string; agreementPublicKey?: string }>();
      if (this.admin(request)) {
        this.ctx.storage.sql.exec("INSERT OR REPLACE INTO pairings (capability, expires_at, candidate_json, wrapped_account_key, approved_device_id, used_at) VALUES (?, ?, NULL, NULL, NULL, NULL)", body.capability, Date.parse(body.expiresAt));
        return Response.json({ accepted: true }, { status: 201 });
      }
      const pairing = [...this.ctx.storage.sql.exec<{ expires_at: number; used_at: number | null }>("SELECT expires_at, used_at FROM pairings WHERE capability = ?", body.capability)][0];
      if (!pairing || pairing.expires_at <= Date.now() || pairing.used_at !== null || !body.deviceId || !body.signingPublicKey || !body.agreementPublicKey) return Response.json({ error: "pairing_rejected" }, { status: 403 });
      const candidate = JSON.stringify({ deviceId: body.deviceId, signingPublicKey: body.signingPublicKey, agreementPublicKey: body.agreementPublicKey });
      this.ctx.storage.sql.exec("UPDATE pairings SET candidate_json = ? WHERE capability = ?", candidate, body.capability);
      return Response.json({ accepted: true });
    }
    if (url.pathname.endsWith("/pairings") && request.method === "GET") {
      const capability = url.searchParams.get("capability") ?? "";
      const pairing = [...this.ctx.storage.sql.exec<{ expires_at: number; candidate_json: string | null; wrapped_account_key: string | null; account_id: string | null; key_version: number | null; approved_device_id: string | null; used_at: number | null }>("SELECT expires_at, candidate_json, wrapped_account_key, account_id, key_version, approved_device_id, used_at FROM pairings WHERE capability = ?", capability)][0];
      if (!pairing || pairing.expires_at <= Date.now()) return Response.json({ error: "pairing_expired" }, { status: 404 });
      if (this.admin(request)) return Response.json({ candidate: pairing.candidate_json ? JSON.parse(pairing.candidate_json) : null, approved: pairing.used_at !== null });
      return Response.json({ approved: pairing.used_at !== null, accountId: pairing.account_id, keyVersion: pairing.key_version ?? 1, deviceId: pairing.approved_device_id, wrappedAccountKey: pairing.wrapped_account_key });
    }
    if (url.pathname.endsWith("/pairings/approve") && request.method === "POST") {
      if (!this.admin(request)) return Response.json({ error: "unauthorized" }, { status: 401 });
      const body = await request.json<{ capability: string; accountId: string; keyVersion: number; deviceId: string; signingPublicKey: string; agreementPublicKey: string; wrappedAccountKey: string }>();
      const pairing = [...this.ctx.storage.sql.exec<{ expires_at: number; used_at: number | null }>("SELECT expires_at, used_at FROM pairings WHERE capability = ?", body.capability)][0];
      if (!pairing || pairing.expires_at <= Date.now() || pairing.used_at !== null) return Response.json({ error: "pairing_rejected" }, { status: 409 });
      this.ctx.storage.sql.exec("INSERT OR REPLACE INTO devices (device_id, signing_public_key, agreement_public_key, revoked_at) VALUES (?, ?, ?, NULL)", body.deviceId, body.signingPublicKey, body.agreementPublicKey);
      this.ctx.storage.sql.exec("UPDATE pairings SET wrapped_account_key = ?, account_id = ?, key_version = ?, approved_device_id = ?, used_at = ? WHERE capability = ?", body.wrappedAccountKey, body.accountId, body.keyVersion, body.deviceId, Date.now(), body.capability);
      return Response.json({ accepted: true });
    }
    if (url.pathname.endsWith("/devices") && request.method === "POST") {
      if (!this.admin(request)) return Response.json({ error: "unauthorized" }, { status: 401 });
      const device = await request.json<{ deviceId: string; signingPublicKey: string; agreementPublicKey: string }>();
      if (!/^dev_[a-z0-9]{16,64}$/.test(device.deviceId)) return Response.json({ error: "invalid_device" }, { status: 400 });
      this.ctx.storage.sql.exec("INSERT OR REPLACE INTO devices (device_id, signing_public_key, agreement_public_key, revoked_at) VALUES (?, ?, ?, NULL)", device.deviceId, device.signingPublicKey, device.agreementPublicKey);
      return Response.json({ accepted: true }, { status: 201 });
    }
    if (url.pathname.endsWith("/revoke") && request.method === "POST") {
      if (!this.admin(request)) return Response.json({ error: "unauthorized" }, { status: 401 });
      const { deviceId } = await request.json<{ deviceId: string }>();
      this.ctx.storage.sql.exec("UPDATE devices SET revoked_at = ? WHERE device_id = ?", Date.now(), deviceId);
      for (const socket of this.ctx.getWebSockets(deviceId)) socket.close(4003, "device revoked");
      return Response.json({ accepted: true });
    }
    if (url.pathname.endsWith("/frames") && request.method === "POST") {
      if (!this.admin(request)) return Response.json({ error: "unauthorized" }, { status: 401 });
      const value = await request.json<RelayFrame>(); let frame: RelayFrame; try { frame = parseRelayFrame(JSON.stringify(value), value.deviceId); } catch { return Response.json({ error: "invalid_opaque_frame" }, { status: 400 }); } if (this.revoked(frame.deviceId)) return Response.json({ error: "device_rejected" }, { status: 403 }); const sequence = this.storeFrame(frame); const outgoing = JSON.stringify({ sequence, ...frame }); for (const peer of this.ctx.getWebSockets()) peer.send(outgoing); return Response.json({ accepted: true, sequence }, { status: 201 });
    }
    if (url.pathname.endsWith("/challenge") && request.method === "POST") {
      const { deviceId } = await request.json<{ deviceId: string }>();
      if (this.revoked(deviceId)) return Response.json({ error: "device_rejected" }, { status: 403 });
      const challenge = randomToken(32); this.ctx.storage.sql.exec("INSERT INTO challenges (challenge, device_id, expires_at, used_at) VALUES (?, ?, ?, NULL)", challenge, deviceId, Date.now() + 60_000);
      this.ctx.storage.sql.exec("DELETE FROM challenges WHERE expires_at < ? OR used_at IS NOT NULL", Date.now() - 60_000);
      return Response.json({ challenge, expiresAt: new Date(Date.now() + 60_000).toISOString() });
    }
    if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") return Response.json({ error: "websocket_required" }, { status: 426 });
    const deviceId = url.searchParams.get("deviceId") ?? "";
    const after = Number(url.searchParams.get("after") ?? "0");
    const challenge = url.searchParams.get("challenge") ?? ""; const signature = url.searchParams.get("signature") ?? "";
    if (!/^dev_[a-z0-9]{16,64}$/.test(deviceId) || !Number.isSafeInteger(after) || after < 0 || !await this.authenticateDevice(this.channelId(url.pathname), deviceId, challenge, signature)) return Response.json({ error: "device_rejected" }, { status: 403 });
    const pair = new WebSocketPair(); const client = pair[0]; const server = pair[1];
    this.ctx.acceptWebSocket(server, [deviceId]);
    for (const row of this.ctx.storage.sql.exec<{ sequence: number; frame_id: string; device_id: string; kind: RelayFrame["kind"]; ciphertext: string }>("SELECT sequence, frame_id, device_id, kind, ciphertext FROM frames WHERE sequence > ? ORDER BY sequence LIMIT 5000", after)) server.send(JSON.stringify({ sequence: row.sequence, frameId: row.frame_id, deviceId: row.device_id, kind: row.kind, ciphertext: row.ciphertext }));
    return new Response(null, { status: 101, webSocket: client });
  }

  webSocketMessage(socket: WebSocket, message: string | ArrayBuffer): void {
    const deviceId = this.ctx.getTags(socket)[0]; if (!deviceId || this.revoked(deviceId)) { socket.close(4003, "device revoked"); return; }
    if (typeof message !== "string") { socket.close(4009, "invalid frame"); return; }
    let frame: RelayFrame;
    try { frame = parseRelayFrame(message, deviceId); } catch { socket.close(4009, "invalid opaque frame"); return; }
    const sequence = this.storeFrame(frame);
    const outgoing = JSON.stringify({ sequence, ...frame });
    for (const peer of this.ctx.getWebSockets()) if (peer !== socket) peer.send(outgoing);
    socket.send(JSON.stringify({ kind: "ack", frameId: frame.frameId, sequence }));
  }

  webSocketClose(socket: WebSocket, code: number, reason: string, wasClean: boolean): void { socket.close(code, reason); void wasClean; }
  private storeFrame(frame: RelayFrame): number { let sequence: number; try { this.ctx.storage.sql.exec("INSERT INTO frames (frame_id, device_id, kind, ciphertext, created_at) VALUES (?, ?, ?, ?, ?)", frame.frameId, frame.deviceId, frame.kind, frame.ciphertext, Date.now()); sequence = [...this.ctx.storage.sql.exec<{ sequence: number }>("SELECT sequence FROM frames WHERE frame_id = ?", frame.frameId)][0]!.sequence; } catch { sequence = [...this.ctx.storage.sql.exec<{ sequence: number }>("SELECT sequence FROM frames WHERE frame_id = ?", frame.frameId)][0]?.sequence ?? 0; } const latest = [...this.ctx.storage.sql.exec<{ sequence: number }>("SELECT COALESCE(MAX(sequence), 0) AS sequence FROM frames")][0]?.sequence ?? sequence; if (frame.kind === "snapshot" && sequence) this.ctx.storage.sql.exec("DELETE FROM frames WHERE kind = 'snapshot' AND sequence != ?", sequence); this.ctx.storage.sql.exec("DELETE FROM frames WHERE sequence < ? AND kind != 'snapshot'", Math.max(0, latest - 5000)); return sequence; }
  private admin(request: Request): boolean { const token = request.headers.get("authorization")?.replace(/^Bearer /, ""); return Boolean(token && token === this.env.RELAY_ADMIN_TOKEN); }
  private revoked(deviceId: string): boolean { const row = [...this.ctx.storage.sql.exec<{ revoked_at: number | null }>("SELECT revoked_at FROM devices WHERE device_id = ?", deviceId)][0]; return !row || row.revoked_at !== null; }
  private channelId(pathname: string): string { return /^\/v1\/channels\/([a-z0-9_-]{16,128})/.exec(pathname)?.[1] ?? ""; }
  private async authenticateDevice(channelId: string, deviceId: string, challenge: string, signature: string): Promise<boolean> {
    if (this.revoked(deviceId)) return false;
    const nonce = [...this.ctx.storage.sql.exec<{ expires_at: number; used_at: number | null }>("SELECT expires_at, used_at FROM challenges WHERE challenge = ? AND device_id = ?", challenge, deviceId)][0];
    const device = [...this.ctx.storage.sql.exec<{ signing_public_key: string }>("SELECT signing_public_key FROM devices WHERE device_id = ?", deviceId)][0];
    if (!nonce || nonce.expires_at <= Date.now() || nonce.used_at !== null || !device) return false;
    try {
      const key = await crypto.subtle.importKey("raw", decode(device.signing_public_key), { name: "Ed25519" }, false, ["verify"]);
      const valid = await crypto.subtle.verify("Ed25519", key, decode(signature), new TextEncoder().encode(`nosh-relay-v1\n${channelId}\n${deviceId}\n${challenge}`));
      if (valid) this.ctx.storage.sql.exec("UPDATE challenges SET used_at = ? WHERE challenge = ?", Date.now(), challenge);
      return valid;
    } catch { return false; }
  }
}

function randomToken(length: number): string { const bytes = new Uint8Array(length); crypto.getRandomValues(bytes); return encode(bytes); }
function encode(bytes: Uint8Array): string { let binary = ""; for (const byte of bytes) binary += String.fromCharCode(byte); return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", ""); }
function decode(value: string): ArrayBuffer { const base64 = value.replaceAll("-", "+").replaceAll("_", "/").padEnd(Math.ceil(value.length / 4) * 4, "="); const binary = atob(base64); const bytes = new Uint8Array(binary.length); for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index); return bytes.buffer; }
