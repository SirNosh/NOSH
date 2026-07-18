const databaseName = "nosh-client";

export async function cacheEncrypted(projectId: string, value: unknown, secret: string): Promise<void> {
  if (!secret) return;
  const key = await keyFrom(secret);
  const nonce = crypto.getRandomValues(new Uint8Array(12));
  const plaintext = new TextEncoder().encode(JSON.stringify(value));
  const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv: nonce }, key, plaintext);
  const database = await open();
  const transaction = database.transaction("snapshots", "readwrite");
  transaction.objectStore("snapshots").put({ projectId, nonce: nonce.buffer, ciphertext, updatedAt: Date.now() });
  await done(transaction);
  database.close();
}

export async function readEncrypted<T>(projectId: string, secret: string): Promise<T | undefined> {
  if (!secret) return undefined;
  const database = await open();
  const transaction = database.transaction("snapshots", "readonly");
  const stored = await request<{ nonce: ArrayBuffer; ciphertext: ArrayBuffer } | undefined>(transaction.objectStore("snapshots").get(projectId));
  database.close();
  if (!stored) return undefined;
  try {
    const plaintext = await crypto.subtle.decrypt({ name: "AES-GCM", iv: new Uint8Array(stored.nonce) }, await keyFrom(secret), stored.ciphertext);
    return JSON.parse(new TextDecoder().decode(plaintext)) as T;
  } catch {
    return undefined;
  }
}

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const opening = indexedDB.open(databaseName, 1);
    opening.onupgradeneeded = () => opening.result.createObjectStore("snapshots", { keyPath: "projectId" });
    opening.onsuccess = () => resolve(opening.result); opening.onerror = () => reject(opening.error);
  });
}
function request<T>(value: IDBRequest<T>): Promise<T> { return new Promise((resolve, reject) => { value.onsuccess = () => resolve(value.result); value.onerror = () => reject(value.error); }); }
function done(transaction: IDBTransaction): Promise<void> { return new Promise((resolve, reject) => { transaction.oncomplete = () => resolve(); transaction.onerror = () => reject(transaction.error); }); }
async function keyFrom(secret: string): Promise<CryptoKey> { return crypto.subtle.importKey("raw", await crypto.subtle.digest("SHA-256", new TextEncoder().encode(secret)), "AES-GCM", false, ["encrypt", "decrypt"]); }
