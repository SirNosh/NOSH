import { cacheEncrypted, readEncrypted } from "./cache.js";
export class RemoteConnection {
    relayUrl;
    channelId;
    keys;
    receive;
    socket;
    cursor;
    stopped = false;
    reconnect;
    inbound = Promise.resolve();
    constructor(relayUrl, channelId, keys, receive) {
        this.relayUrl = relayUrl;
        this.channelId = channelId;
        this.keys = keys;
        this.receive = receive;
        this.cursor = Number(localStorage.getItem(`nosh.remote.cursor.${channelId}`) ?? "0");
    }
    async connect() { this.stopped = false; const modules = await import("@nosh/crypto"); const challengeResponse = await fetch(new URL(`/v1/channels/${this.channelId}/challenge`, this.relayUrl), { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ deviceId: this.keys.deviceId }) }); if (!challengeResponse.ok)
        throw new Error("Relay authentication challenge failed"); const { challenge } = await challengeResponse.json(); const signature = await modules.signRelayChallenge(this.channelId, challenge, this.keys); const url = new URL(`/v1/channels/${this.channelId}`, this.relayUrl); url.protocol = url.protocol === "https:" ? "wss:" : "ws:"; url.searchParams.set("deviceId", this.keys.deviceId); url.searchParams.set("after", String(this.cursor)); url.searchParams.set("challenge", challenge); url.searchParams.set("signature", signature); const socket = new WebSocket(url); this.socket = socket; await new Promise((resolve, reject) => { socket.onopen = () => resolve(); socket.onerror = () => reject(new Error("Relay WebSocket connection failed")); }); socket.onmessage = (message) => { const frame = JSON.parse(String(message.data)); this.inbound = this.inbound.then(async () => { await this.receive(frame); if (frame.sequence && frame.sequence > this.cursor) {
        this.cursor = frame.sequence;
        localStorage.setItem(`nosh.remote.cursor.${this.channelId}`, String(this.cursor));
    } }).catch(() => undefined); }; socket.onclose = () => { if (!this.stopped)
        this.reconnect = window.setTimeout(() => void this.connect().catch(() => undefined), 2_000); }; }
    send(kind, ciphertext) { if (this.socket?.readyState !== WebSocket.OPEN)
        return false; this.socket.send(JSON.stringify({ frameId: `frm_${crypto.randomUUID().replaceAll("-", "")}`, deviceId: this.keys.deviceId, kind, ciphertext })); return true; }
    async sendCommand(command, accountKey) { const { sealRelayPayload } = await import("@nosh/crypto"); return this.send("command", await sealRelayPayload(command, accountKey)); }
    close() { this.stopped = true; if (this.reconnect)
        clearTimeout(this.reconnect); this.socket?.close(); }
}
class RemoteSession {
    relayUrl;
    channelId;
    vault;
    persist;
    snapshot = null;
    accountKey;
    connection;
    pending = new Map();
    listeners = new Set();
    constructor(relayUrl, channelId, vault, persist = () => undefined) {
        this.relayUrl = relayUrl;
        this.channelId = channelId;
        this.vault = vault;
        this.persist = persist;
        this.accountKey = decode(vault.accountKey);
        this.connection = new RemoteConnection(relayUrl, channelId, vault, (frame) => this.receive(frame));
    }
    async start() { this.snapshot = await readEncrypted("remote", this.vault.accountKey) ?? null; try {
        await this.connection.connect();
    }
    catch (error) {
        if (!this.snapshot)
            throw error;
    } if (!this.snapshot)
        for (let attempt = 0; attempt < 50 && !this.snapshot; attempt += 1)
            await new Promise((resolve) => setTimeout(resolve, 100)); if (!this.snapshot)
        throw new Error("Connected, but no encrypted Project snapshot arrived"); }
    stop() { this.connection.close(); }
    subscribe(listener) { this.listeners.add(listener); return () => this.listeners.delete(listener); }
    async api(path, init) {
        const url = new URL(path, "https://nosh.invalid");
        const projectId = url.searchParams.get("projectId") ?? "";
        const method = init?.method ?? "GET";
        if (method === "GET") {
            const snapshot = this.requiredSnapshot();
            if (url.pathname === "/projects")
                return { projects: snapshot.projects };
            if (url.pathname === "/models")
                return { models: [...new Map(snapshot.agents.filter((agent) => agent.modelProvider && agent.modelId).map((agent) => [`${agent.modelProvider}/${agent.modelId}`, { provider: agent.modelProvider, id: agent.modelId, name: agent.modelName ?? agent.modelId }])).values()] };
            if (url.pathname === "/events")
                return { events: snapshot.events[projectId] ?? [] };
            if (url.pathname === "/missions")
                return { missions: snapshot.missions[projectId] ?? [] };
            if (url.pathname === "/directions")
                return { directions: snapshot.directions[projectId] ?? [] };
            if (url.pathname === "/autoresearch")
                return { executions: snapshot.autoresearch[projectId] ?? [] };
            if (url.pathname === "/threads")
                return { threads: snapshot.threads?.[projectId] ?? [] };
            if (url.pathname === "/records") {
                const schema = url.searchParams.get("schema");
                return { records: (snapshot.records[projectId] ?? []).filter((record) => !schema || record.$schema === `https://nosh.dev/schemas/${schema}/v1`) };
            }
            if (url.pathname === "/paper")
                return (snapshot.papers[projectId] ?? { markdown: "", bibliography: "" });
            if (url.pathname === "/agents")
                return { agents: snapshot.agents };
            if (url.pathname === "/jobs")
                return { jobs: snapshot.jobs };
        }
        const steering = /^\/missions\/([^/]+)\/steer$/.exec(url.pathname);
        if (method === "POST" && steering?.[1]) {
            const body = JSON.parse(String(init?.body ?? "{}"));
            await this.command(body.projectId, "mission", steering[1], body.expectedVersion, "mission.steer", { message: body.message });
            return { accepted: true };
        }
        const control = /^\/missions\/([^/]+)\/control$/.exec(url.pathname);
        if (method === "POST" && control?.[1]) {
            const body = JSON.parse(String(init?.body ?? "{}"));
            const operation = body.action === "pause" ? "mission.pause" : body.action === "stop" ? "mission.stop" : "mission.resume";
            const ack = await this.command(body.projectId, "mission", control[1], body.expectedVersion, operation, operation === "mission.resume" ? {} : { mode: body.mode });
            const stored = this.requiredSnapshot().missions[body.projectId]?.find((item) => item.entityId === control[1]);
            if (stored) {
                stored.version = ack.currentVersion ?? stored.version;
                stored.state = body.action === "pause" ? "paused" : body.action === "stop" ? "stopped" : "running";
                stored.value.state = stored.state;
            }
            return { mission: stored };
        }
        const mission = /^\/missions\/([^/]+)\/transition$/.exec(url.pathname);
        if (method === "POST" && mission?.[1]) {
            const body = JSON.parse(String(init?.body ?? "{}"));
            const operation = body.next === "running" ? "mission.resume" : ["pausing", "paused"].includes(body.next) ? "mission.pause" : ["stopping", "stopped"].includes(body.next) ? "mission.stop" : null;
            if (!operation)
                throw new Error("This Mission transition requires local approval and is unavailable remotely");
            const payload = operation === "mission.pause" ? { mode: "safe" } : operation === "mission.stop" ? { mode: "safe" } : {};
            const ack = await this.command(body.projectId, "mission", mission[1], body.expectedVersion, operation, payload);
            const stored = this.requiredSnapshot().missions[body.projectId]?.find((item) => item.entityId === mission[1]);
            if (stored) {
                stored.version = ack.currentVersion ?? stored.version;
                stored.state = operation === "mission.pause" ? "paused" : operation === "mission.stop" ? "stopped" : "running";
                stored.value.state = stored.state;
            }
            return { mission: stored };
        }
        const message = /^\/threads\/([^/]+)\/messages$/.exec(url.pathname);
        if (method === "POST" && message?.[1]) {
            const body = JSON.parse(String(init?.body ?? "{}"));
            const stored = this.thread(body.projectId, message[1]);
            const ack = await this.command(body.projectId, "thread", message[1], stored.version, "thread.message", { message: body.message });
            stored.version = ack.currentVersion ?? stored.version;
            return { accepted: true };
        }
        if (method === "POST" && url.pathname === "/runtime/instructions") {
            const body = JSON.parse(String(init?.body ?? "{}"));
            if (!body.projectId || !body.threadId || body.operation !== "STOP" || body.programId)
                throw new Error("Only stopping a foreground fork is available remotely");
            const stored = this.thread(body.projectId, body.threadId);
            const ack = await this.command(body.projectId, "thread", body.threadId, stored.version, "thread.stop", {});
            stored.version = ack.currentVersion ?? stored.version;
            stored.state = "completed";
            stored.value.state = "completed";
            return { accepted: true };
        }
        throw new Error("This operation is read-only or unavailable while using the remote snapshot");
    }
    async command(projectId, targetType, targetId, expectedVersion, type, payload) { const { encryptCommand } = await import("@nosh/crypto"); const commandId = `cmd_${crypto.randomUUID().replaceAll("-", "")}`; const command = await encryptCommand({ protocolVersion: 1, accountId: this.vault.accountId, commandId, idempotencyKey: `remote-${crypto.randomUUID()}`, deviceId: this.vault.deviceId, projectId, targetType, targetId, expectedVersion, type, requiredPermission: type, keyVersion: this.vault.keyVersion ?? 1, issuedAt: new Date().toISOString(), expiresAt: new Date(Date.now() + 60_000).toISOString() }, payload, this.accountKey, this.vault.signingSecretKey); const result = new Promise((resolve, reject) => { const timer = window.setTimeout(() => { this.pending.delete(commandId); reject(new Error("No live noshd acknowledgement; command execution is unconfirmed")); }, type === "thread.message" ? 600_000 : 20_000); this.pending.set(commandId, { resolve, reject, timer }); }); if (!await this.connection.sendCommand(command, this.accountKey)) {
        const pending = this.pending.get(commandId);
        if (pending) {
            clearTimeout(pending.timer);
            this.pending.delete(commandId);
            pending.reject(new Error("Workstation is offline; command was not executed"));
        }
    } return result; }
    async receive(frame) { if (!frame.ciphertext || !["snapshot", "event", "ack"].includes(frame.kind))
        return; const { openRelayPayload, unlockWrappedAccountKey } = await import("@nosh/crypto"); const value = await openRelayPayload(frame.ciphertext, this.accountKey); const rotation = value; if (rotation.type === "nosh.key-rotation.v1") {
        if (rotation.deviceId !== this.vault.deviceId || !rotation.keyVersion || !rotation.wrappedAccountKey)
            return;
        const next = await unlockWrappedAccountKey(rotation.wrappedAccountKey, this.vault);
        this.accountKey.fill(0);
        this.accountKey = next;
        this.vault.accountKey = encode(next);
        this.vault.keyVersion = rotation.keyVersion;
        await this.persist(this.vault);
        return;
    } if (frame.kind === "snapshot" && value.type === "nosh.snapshot.v1") {
        this.snapshot = value;
        await cacheEncrypted("remote", this.snapshot, this.vault.accountKey);
        window.dispatchEvent(new Event("nosh-remote-snapshot"));
        return;
    } if (frame.kind === "event") {
        const event = value;
        const snapshot = this.requiredSnapshot();
        const events = snapshot.events[event.scope.projectId] ?? [];
        snapshot.events[event.scope.projectId] = [...events.filter((item) => item.eventId !== event.eventId), event].slice(-200);
        const payload = event.payload;
        if (event.scope.missionId && payload.state) {
            const mission = snapshot.missions[event.scope.projectId]?.find((item) => item.entityId === event.scope.missionId);
            if (mission) {
                mission.state = payload.state;
                mission.value.state = payload.state;
                mission.version = payload.version ?? mission.version;
            }
        }
        await cacheEncrypted("remote", snapshot, this.vault.accountKey);
        for (const listener of this.listeners)
            listener(event);
        return;
    } const ack = value; if (ack.commandId) {
        const pending = this.pending.get(ack.commandId);
        if (pending) {
            clearTimeout(pending.timer);
            this.pending.delete(ack.commandId);
            if (ack.accepted)
                pending.resolve(ack);
            else
                pending.reject(new Error(ack.error ?? "Remote command rejected"));
        }
    } }
    requiredSnapshot() { if (!this.snapshot)
        throw new Error("No encrypted Project snapshot is available yet"); return this.snapshot; }
    thread(projectId, threadId) { const thread = this.requiredSnapshot().threads?.[projectId]?.find((item) => item.entityId === threadId); if (!thread || thread.value.executionMode !== "foreground_fork")
        throw new Error("Foreground thread is unavailable in the encrypted snapshot"); return thread; }
}
let active = null;
export function remoteActive() { return active !== null; }
export async function startRemoteSession(relayUrl, channelId, vault, persist) { active?.stop(); const session = new RemoteSession(relayUrl, channelId, vault, persist); await session.start(); active = session; }
export function remoteApi(path, init) { if (!active)
    return Promise.reject(new Error("Remote vault is locked")); return active.api(path, init); }
export function subscribeRemote(receive) { return active?.subscribe(receive) ?? (() => undefined); }
export async function presentPairing(relayUrl, channelId, input) { const response = await fetch(new URL(`/v1/channels/${channelId}/pairings`, relayUrl), { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(input) }); if (!response.ok)
    throw new Error("Pairing capability was rejected"); }
export async function finishPairing(relayUrl, channelId, capability, keys) { const response = await fetch(new URL(`/v1/channels/${channelId}/pairings?capability=${encodeURIComponent(capability)}`, relayUrl)); if (!response.ok)
    throw new Error("Pairing has not been approved"); const result = await response.json(); if (!result.approved || !result.accountId || !result.wrappedAccountKey)
    throw new Error("Pairing is still awaiting Windows approval"); const { unlockWrappedAccountKey } = await import("@nosh/crypto"); return { accountId: result.accountId, accountKey: await unlockWrappedAccountKey(result.wrappedAccountKey, keys), keyVersion: result.keyVersion ?? 1 }; }
export async function unlockPairedVault(vault, password) { const { decryptVault } = await import("@nosh/crypto"); const value = await decryptVault(vault, password); if (!value.accountId || !value.accountKey || !value.signingSecretKey)
    throw new Error("Device pairing is incomplete"); return value; }
function decode(value) { const base64 = value.replaceAll("-", "+").replaceAll("_", "/").padEnd(Math.ceil(value.length / 4) * 4, "="); const binary = atob(base64); return Uint8Array.from(binary, (character) => character.charCodeAt(0)); }
function encode(value) { return btoa(String.fromCharCode(...value)).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", ""); }
//# sourceMappingURL=remote.js.map