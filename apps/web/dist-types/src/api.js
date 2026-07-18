import { remoteActive, remoteApi, subscribeRemote } from "./remote.js";
export function token() { return sessionStorage.getItem("nosh.sessionToken") ?? localStorage.getItem("nosh.bootstrapToken") ?? ""; }
let exchange = null;
async function localToken() { const session = sessionStorage.getItem("nosh.sessionToken"); const expiresAt = Date.parse(sessionStorage.getItem("nosh.sessionExpiresAt") ?? ""); if (session && expiresAt > Date.now() + 5_000)
    return session; if (exchange)
    return exchange; exchange = (async () => { const bootstrap = localStorage.getItem("nosh.bootstrapToken"); if (!bootstrap)
    throw new Error("Enter the loopback bootstrap token in Settings"); const response = await fetch("/api/session", { method: "POST", headers: { authorization: `Bearer ${bootstrap}` } }); if (!response.ok)
    throw new Error("Local session exchange failed"); const value = await response.json(); sessionStorage.setItem("nosh.sessionToken", value.token); sessionStorage.setItem("nosh.sessionExpiresAt", value.expiresAt); localStorage.removeItem("nosh.bootstrapToken"); return value.token; })().finally(() => { exchange = null; }); return exchange; }
export async function api(path, init) {
    if (remoteActive())
        return remoteApi(path, init);
    const response = await fetch(`/api${path}`, { ...init, headers: { authorization: `Bearer ${await localToken()}`, "content-type": "application/json", ...init?.headers } });
    if (!response.ok)
        throw new Error((await response.json()).error ?? `Request failed (${response.status})`);
    return response.json();
}
export function subscribe(projectId, after, receive) {
    if (remoteActive())
        return subscribeRemote((event) => { if (event.scope.projectId === projectId && (event.sequence === null || event.sequence > after))
            receive(event); });
    if (!projectId || !token())
        return () => undefined;
    let socket;
    let closed = false;
    void localToken().then((session) => { if (closed)
        return; const url = new URL(`/api/events?projectId=${encodeURIComponent(projectId)}&after=${after}`, window.location.href); url.protocol = url.protocol === "https:" ? "wss:" : "ws:"; socket = new WebSocket(url, ["nosh", `auth.${session}`]); socket.onmessage = (message) => receive(JSON.parse(String(message.data))); }).catch(() => undefined);
    return () => { closed = true; socket?.close(); };
}
//# sourceMappingURL=api.js.map