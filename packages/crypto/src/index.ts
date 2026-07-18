import sodium from "libsodium-wrappers-sumo";
import { canonicalJson, parseRemoteCommandPayload, remoteCommandEnvelopeSchema, remoteCommandPolicy, type JsonValue, type RemoteCommandEnvelope } from "@nosh/wire";

export type DeviceKeys = { deviceId: string; signingPublicKey: string; signingSecretKey: string; agreementPublicKey: string; agreementSecretKey: string };
export type DeviceRecord = { deviceId: string; signingPublicKey: string; agreementPublicKey: string; permissions: string[]; approvedAt: string; revokedAt: string | null };
export type PairingCapability = { capability: string; verificationCode: string; expiresAt: string; used: boolean; candidate: Omit<DeviceRecord, "permissions" | "approvedAt" | "revokedAt"> | null };
export type Vault = { version: 1; salt: string; nonce: string; ciphertext: string };
export type DeviceRegistryState = { capabilities: PairingCapability[]; devices: DeviceRecord[] };

export async function createDeviceKeys(): Promise<DeviceKeys> {
  await sodium.ready;
  const signing = sodium.crypto_sign_keypair(); const agreement = sodium.crypto_box_keypair();
  const deviceId = `dev_${sodium.to_hex(sodium.randombytes_buf(16))}`;
  return { deviceId, signingPublicKey: b64(signing.publicKey), signingSecretKey: b64(signing.privateKey), agreementPublicKey: b64(agreement.publicKey), agreementSecretKey: b64(agreement.privateKey) };
}

export class DeviceRegistry {
  private capabilities = new Map<string, PairingCapability>(); private devices = new Map<string, DeviceRecord>();
  constructor(state?: DeviceRegistryState) { for (const pairing of state?.capabilities ?? []) this.capabilities.set(pairing.capability, structuredClone(pairing)); for (const device of state?.devices ?? []) this.devices.set(device.deviceId, structuredClone(device)); }
  async createPairing(ttlSeconds = 300): Promise<PairingCapability> { await sodium.ready; const raw = sodium.randombytes_buf(24); const capability = b64(raw); const hash = sodium.crypto_generichash(4, raw, null); const verificationCode = String((Number.parseInt(sodium.to_hex(hash), 16) % 1_000_000)).padStart(6, "0"); const record: PairingCapability = { capability, verificationCode, expiresAt: new Date(Date.now() + ttlSeconds * 1000).toISOString(), used: false, candidate: null }; this.capabilities.set(capability, record); return structuredClone(record); }
  present(capability: string, candidate: PairingCapability["candidate"]): PairingCapability { const pairing = this.requiredCapability(capability); if (pairing.used || Date.parse(pairing.expiresAt) <= Date.now()) throw new Error("Pairing capability expired or used"); if (!candidate) throw new Error("Device public keys are required"); pairing.candidate = structuredClone(candidate); return structuredClone(pairing); }
  approve(capability: string, verificationCode: string, permissions: string[], accountKey: Uint8Array): { device: DeviceRecord; wrappedAccountKey: string } { const pairing = this.requiredCapability(capability); if (pairing.used || !pairing.candidate || pairing.verificationCode !== verificationCode || Date.parse(pairing.expiresAt) <= Date.now()) throw new Error("Pairing approval failed"); const device: DeviceRecord = { ...pairing.candidate, permissions: [...permissions], approvedAt: new Date().toISOString(), revokedAt: null }; pairing.used = true; this.devices.set(device.deviceId, device); return { device: structuredClone(device), wrappedAccountKey: b64(sodium.crypto_box_seal(accountKey, from64(device.agreementPublicKey))) }; }
  revoke(deviceId: string): void { const device = this.requiredDevice(deviceId); device.revokedAt = new Date().toISOString(); }
  get(deviceId: string): DeviceRecord { return structuredClone(this.requiredDevice(deviceId)); }
  snapshot(): DeviceRegistryState { return { capabilities: [...this.capabilities.values()].map((value) => structuredClone(value)), devices: [...this.devices.values()].map((value) => structuredClone(value)) }; }
  private requiredCapability(capability: string): PairingCapability { const pairing = this.capabilities.get(capability); if (!pairing) throw new Error("Unknown pairing capability"); return pairing; }
  private requiredDevice(deviceId: string): DeviceRecord { const device = this.devices.get(deviceId); if (!device) throw new Error("Unknown device"); return device; }
}

export async function unlockWrappedAccountKey(wrapped: string, keys: DeviceKeys): Promise<Uint8Array> { await sodium.ready; const opened = sodium.crypto_box_seal_open(from64(wrapped), from64(keys.agreementPublicKey), from64(keys.agreementSecretKey)); if (!opened) throw new Error("Account key could not be unwrapped"); return opened; }
export async function wrapAccountKey(accountKey: Uint8Array, agreementPublicKey: string): Promise<string> { await sodium.ready; return b64(sodium.crypto_box_seal(accountKey, from64(agreementPublicKey))); }

export async function encryptVault(value: JsonValue, password: string): Promise<Vault> { await sodium.ready; const salt = sodium.randombytes_buf(sodium.crypto_pwhash_SALTBYTES); const key = sodium.crypto_pwhash(sodium.crypto_aead_xchacha20poly1305_ietf_KEYBYTES, password, salt, sodium.crypto_pwhash_OPSLIMIT_INTERACTIVE, sodium.crypto_pwhash_MEMLIMIT_INTERACTIVE, sodium.crypto_pwhash_ALG_ARGON2ID13); const nonce = sodium.randombytes_buf(sodium.crypto_aead_xchacha20poly1305_ietf_NPUBBYTES); const ciphertext = sodium.crypto_aead_xchacha20poly1305_ietf_encrypt(canonicalJson(value), null, null, nonce, key); sodium.memzero(key); return { version: 1, salt: b64(salt), nonce: b64(nonce), ciphertext: b64(ciphertext) }; }
export async function decryptVault(vault: Vault, password: string): Promise<JsonValue> { await sodium.ready; const key = sodium.crypto_pwhash(sodium.crypto_aead_xchacha20poly1305_ietf_KEYBYTES, password, from64(vault.salt), sodium.crypto_pwhash_OPSLIMIT_INTERACTIVE, sodium.crypto_pwhash_MEMLIMIT_INTERACTIVE, sodium.crypto_pwhash_ALG_ARGON2ID13); const plaintext = sodium.crypto_aead_xchacha20poly1305_ietf_decrypt(null, from64(vault.ciphertext), null, from64(vault.nonce), key); sodium.memzero(key); return JSON.parse(sodium.to_string(plaintext)) as JsonValue; }

export async function sealRelayPayload(value: JsonValue, dataKey: Uint8Array): Promise<string> { await sodium.ready; const nonce = sodium.randombytes_buf(sodium.crypto_aead_xchacha20poly1305_ietf_NPUBBYTES); const ciphertext = sodium.crypto_aead_xchacha20poly1305_ietf_encrypt(canonicalJson(value), "nosh-relay-v1", null, nonce, dataKey); const bytes = new Uint8Array(nonce.length + ciphertext.length); bytes.set(nonce); bytes.set(ciphertext, nonce.length); return b64(bytes); }
export async function openRelayPayload(value: string, dataKey: Uint8Array): Promise<JsonValue> { await sodium.ready; const bytes = from64(value); const nonceBytes = sodium.crypto_aead_xchacha20poly1305_ietf_NPUBBYTES; if (bytes.length <= nonceBytes) throw new Error("Invalid relay ciphertext"); const plaintext = sodium.crypto_aead_xchacha20poly1305_ietf_decrypt(null, bytes.slice(nonceBytes), "nosh-relay-v1", bytes.slice(0, nonceBytes), dataKey); return JSON.parse(sodium.to_string(plaintext)) as JsonValue; }
export async function signRelayChallenge(channelId: string, challenge: string, keys: DeviceKeys): Promise<string> { await sodium.ready; return b64(sodium.crypto_sign_detached(relayChallengeMessage(channelId, keys.deviceId, challenge), from64(keys.signingSecretKey))); }
export function relayChallengeMessage(channelId: string, deviceId: string, challenge: string): string { return `nosh-relay-v1\n${channelId}\n${deviceId}\n${challenge}`; }

type CommandHeader = Omit<RemoteCommandEnvelope, "nonce" | "ciphertext" | "signature">;
export async function encryptCommand(header: CommandHeader, payload: JsonValue, dataKey: Uint8Array, signingSecretKey: string): Promise<RemoteCommandEnvelope> { await sodium.ready; const parsed = remoteCommandEnvelopeSchema.omit({ nonce: true, ciphertext: true, signature: true }).parse(header); const nonce = sodium.randombytes_buf(sodium.crypto_aead_xchacha20poly1305_ietf_NPUBBYTES); const associated = canonicalJson(parsed); const ciphertext = sodium.crypto_aead_xchacha20poly1305_ietf_encrypt(canonicalJson(payload), associated, null, nonce, dataKey); const unsigned = { ...parsed, nonce: b64(nonce), ciphertext: b64(ciphertext) }; const signature = sodium.crypto_sign_detached(canonicalJson(unsigned), from64(signingSecretKey)); return remoteCommandEnvelopeSchema.parse({ ...unsigned, signature: b64(signature) }); }

export class RemoteCommandValidator {
  private receipts = new Map<string, { envelope: string; payload: JsonValue }>();
  constructor(private readonly registry: DeviceRegistry, private readonly dataKey: Uint8Array, private readonly accountId: string, private readonly versions: (command: RemoteCommandEnvelope) => number, private readonly persistedReplay: (command: RemoteCommandEnvelope) => boolean = () => false, private readonly keyVersion = 1) {}
  async validate(envelope: RemoteCommandEnvelope): Promise<{ replayed: boolean; payload: JsonValue }> {
    await sodium.ready; const command = remoteCommandEnvelopeSchema.parse(envelope);
    if (command.accountId !== this.accountId) throw new Error("Command account scope does not match");
    if (command.keyVersion !== this.keyVersion) throw new Error("Command key version is stale");
    const policy = remoteCommandPolicy[command.type]; if (command.targetType !== policy.targetType || command.requiredPermission !== policy.permission) throw new Error("Command capability does not match its semantic operation");
    const device = this.registry.get(command.deviceId); if (device.revokedAt) throw new Error("Device revoked"); if (!device.permissions.includes(policy.permission)) throw new Error("Device permission denied");
    const now = Date.now(); if (Date.parse(command.issuedAt) > now + 30_000 || Date.parse(command.expiresAt) <= now) throw new Error("Command is expired or issued in the future");
    const { signature, ...unsigned } = command; if (!sodium.crypto_sign_verify_detached(from64(signature), canonicalJson(unsigned), from64(device.signingPublicKey))) throw new Error("Invalid device signature");
    const { nonce, ciphertext, ...header } = unsigned; const plaintext = sodium.crypto_aead_xchacha20poly1305_ietf_decrypt(null, from64(ciphertext), canonicalJson(header), from64(nonce), this.dataKey); const payload = parseRemoteCommandPayload(command.type, JSON.parse(sodium.to_string(plaintext)));
    const encoded = canonicalJson(command); const prior = this.receipts.get(command.idempotencyKey); if (prior) { if (prior.envelope !== encoded) throw new Error("Idempotency key was reused for a different command"); return { replayed: true, payload: prior.payload }; }
    if (this.persistedReplay(command)) { this.receipts.set(command.idempotencyKey, { envelope: encoded, payload }); return { replayed: true, payload }; }
    if (this.versions(command) !== command.expectedVersion) throw new Error("Optimistic concurrency conflict");
    this.receipts.set(command.idempotencyKey, { envelope: encoded, payload }); return { replayed: false, payload };
  }
}

function b64(value: Uint8Array): string { return sodium.to_base64(value, sodium.base64_variants.URLSAFE_NO_PADDING); }
function from64(value: string): Uint8Array { return sodium.from_base64(value, sodium.base64_variants.URLSAFE_NO_PADDING); }
