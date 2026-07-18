import type { DeviceKeys, Vault } from "@nosh/crypto";
import type { RemoteCommandEnvelope } from "@nosh/wire";
import type { NoshEvent } from "./api.js";
export type RemoteFrame = {
    sequence?: number;
    frameId: string;
    deviceId: string;
    kind: "event" | "snapshot" | "command" | "ack";
    ciphertext?: string;
};
export type PairedVault = DeviceKeys & {
    accountId: string;
    accountKey: string;
    keyVersion?: number;
};
export declare class RemoteConnection {
    private readonly relayUrl;
    private readonly channelId;
    private readonly keys;
    private readonly receive;
    private socket;
    private cursor;
    private stopped;
    private reconnect;
    private inbound;
    constructor(relayUrl: string, channelId: string, keys: DeviceKeys, receive: (frame: RemoteFrame) => void | Promise<void>);
    connect(): Promise<void>;
    send(kind: "command" | "ack", ciphertext: string): boolean;
    sendCommand(command: RemoteCommandEnvelope, accountKey: Uint8Array): Promise<boolean>;
    close(): void;
}
export declare function remoteActive(): boolean;
export declare function startRemoteSession(relayUrl: string, channelId: string, vault: PairedVault, persist?: (vault: PairedVault) => void | Promise<void>): Promise<void>;
export declare function remoteApi<T>(path: string, init?: RequestInit): Promise<T>;
export declare function subscribeRemote(receive: (event: NoshEvent) => void): () => void;
export declare function presentPairing(relayUrl: string, channelId: string, input: {
    capability: string;
    deviceId: string;
    signingPublicKey: string;
    agreementPublicKey: string;
}): Promise<void>;
export declare function finishPairing(relayUrl: string, channelId: string, capability: string, keys: DeviceKeys): Promise<{
    accountId: string;
    accountKey: Uint8Array;
    keyVersion: number;
}>;
export declare function unlockPairedVault(vault: Vault, password: string): Promise<PairedVault>;
