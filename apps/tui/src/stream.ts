import { Buffer } from 'node:buffer';

/** Raw daemon envelopes. Ephemeral Pi events have a null durable sequence. */
export interface StreamEvent {
  eventId: string;
  sequence: number | null;
  type: string;
  payload: Record<string, unknown>;
  scope: { projectId: string; agentId?: string | null; [key: string]: unknown };
  correlationId?: string | null;
  retention?: 'persistent' | 'ephemeral';
}
export type StreamState = 'connecting' | 'connected' | 'disconnected';
export type EventSocket = Pick<WebSocket, 'onopen' | 'onmessage' | 'onerror' | 'onclose' | 'close'>;
export type WebSocketFactory = (url: string, protocols: string[]) => EventSocket;

export const MAX_EVENT_BYTES = 1024 * 1024;
const MAX_SEEN_IDS = 512;
const CONNECT_TIMEOUT_MS = 15_000;
const RETRY_BASE_MS = 250;
const RETRY_MAX_MS = 10_000;
const STABLE_CONNECTION_MS = 5_000;

interface StreamOptions {
  baseUrl: string;
  projectId: string;
  after: number;
  getToken: (refresh: boolean) => Promise<string>;
  createSocket: WebSocketFactory;
  onEvent: (event: StreamEvent) => void;
  onState: (state: StreamState) => void;
}

/**
 * Subscribe only after the recent HTTP bootstrap. The server attaches its live
 * listener before replaying durable events after this cursor. Never infer gaps
 * from nonconsecutive sequences: they are global, not project-local.
 *
 * No frame queue is kept here. Consumers ingest synchronously and coalesce their
 * own rendering (at most 20 fps). Keep HTTP polling as the durable fallback.
 */
export function subscribeEvents(options: StreamOptions): () => void {
  if (!options.projectId || options.projectId.length > 1024) throw new Error('Invalid stream project');
  if (!Number.isSafeInteger(options.after) || options.after < 0) throw new Error('Invalid stream cursor');
  const url = new URL(options.baseUrl + '/events');
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash)
    throw new Error('Invalid stream URL');
  if (url.protocol !== 'https:' && !['localhost', '127.0.0.1', '[::1]'].includes(url.hostname))
    throw new Error('Non-loopback daemon connections require HTTPS');
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.searchParams.set('projectId', options.projectId);
  let cursor = options.after;
  let stopped = false;
  let generation = 0;
  let attempted = false;
  let failures = 0;
  let openedAt: number | undefined;
  let socket: EventSocket | undefined;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let connectTimer: ReturnType<typeof setTimeout> | undefined;
  const seen = new Set<string>();
  const active = (id: number) => !stopped && id === generation;
  const state = (value: StreamState) => {
    // Consumer errors must not leak an uncaught rejection or stop cleanup.
    try { options.onState(value); } catch { /* polling remains available */ }
  };
  const releaseSocket = () => {
    clearTimeout(connectTimer); connectTimer = undefined;
    const previous = socket; socket = undefined;
    if (!previous) return;
    previous.onopen = previous.onmessage = previous.onclose = previous.onerror = null;
    try { previous.close(1000, 'Stream detached'); } catch { /* already closed */ }
  };
  const failed = (id: number) => {
    if (!active(id)) return;
    generation++;
    releaseSocket();
    state('disconnected'); // All ephemeral drafts must be discarded, not replayed.
    if (stopped) return;
    if (openedAt !== undefined && Date.now() - openedAt >= STABLE_CONNECTION_MS) failures = 0;
    openedAt = undefined;
    const delay = Math.min(RETRY_BASE_MS * 2 ** Math.min(failures++, 6), RETRY_MAX_MS);
    retryTimer = setTimeout(() => { retryTimer = undefined; void connect(); }, delay);
    retryTimer.unref?.();
  };
  const connect = async () => {
    if (stopped) return;
    const id = ++generation;
    state('connecting');
    if (!active(id)) return;
    try {
      const refresh = attempted; attempted = true;
      const token = await options.getToken(refresh);
      if (!active(id)) return;
      // RFC 6455 subprotocol token syntax. Never put credentials in URLs/errors.
      if (!token || token.length > 8192 || !/^[!#$%&'*+.^_`|~0-9a-z-]+$/i.test(token))
        throw new Error('Invalid session token');
      url.searchParams.set('after', String(cursor));
      const current = options.createSocket(url.href, [`auth.${token}`]);
      socket = current;
      current.onopen = () => {
        if (!active(id)) return;
        clearTimeout(connectTimer); connectTimer = undefined;
        openedAt = Date.now();
        state('connected');
      };
      current.onerror = current.onclose = () => failed(id);
      current.onmessage = (message) => {
        if (!active(id)) return;
        try {
          // Daemon frames are text JSON. Reject binary without copying or queuing.
          if (typeof message.data !== 'string' || message.data.length > MAX_EVENT_BYTES ||
              Buffer.byteLength(message.data, 'utf8') > MAX_EVENT_BYTES) throw new Error('Invalid frame');
          const event: unknown = JSON.parse(message.data);
          if (!isStreamEvent(event)) throw new Error('Invalid event');
          if (event.scope.projectId !== options.projectId || seen.has(event.eventId)) return;
          if (event.sequence !== null && event.sequence <= cursor) return;
          options.onEvent(event);
          if (!active(id)) return;
          // Ack only after successful synchronous ingestion; thrown callbacks replay.
          if (event.sequence !== null) cursor = event.sequence;
          seen.add(event.eventId);
          if (seen.size > MAX_SEEN_IDS) seen.delete(seen.values().next().value!);
        } catch { failed(id); }
      };
      connectTimer = setTimeout(() => failed(id), CONNECT_TIMEOUT_MS);
      connectTimer.unref?.();
    } catch { failed(id); }
  };
  // Defer admission so callers can store cleanup before any state callbacks.
  queueMicrotask(() => { void connect(); });
  return () => {
    if (stopped) return;
    stopped = true; generation++;
    clearTimeout(retryTimer); retryTimer = undefined;
    releaseSocket(); seen.clear();
    state('disconnected');
  };
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
function isStreamEvent(value: unknown): value is StreamEvent {
  if (!record(value) || !record(value.scope) || !record(value.payload)) return false;
  if (typeof value.eventId !== 'string' || !value.eventId || value.eventId.length > 256 ||
      typeof value.type !== 'string' || !value.type || value.type.length > 128 ||
      typeof value.scope.projectId !== 'string' || value.scope.projectId.length > 1024) return false;
  if (value.sequence !== null && (!Number.isSafeInteger(value.sequence) || (value.sequence as number) < 1)) return false;
  if (value.retention !== undefined && value.retention !== (value.sequence === null ? 'ephemeral' : 'persistent')) return false;
  if (value.scope.agentId !== undefined && value.scope.agentId !== null && typeof value.scope.agentId !== 'string') return false;
  if (value.correlationId !== undefined && value.correlationId !== null && typeof value.correlationId !== 'string') return false;
  return true;
}
