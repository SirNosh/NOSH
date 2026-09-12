import { subscribeEvents, type StreamEvent, type StreamState, type WebSocketFactory } from './stream.js';
export type { StreamEvent, StreamState, WebSocketFactory } from './stream.js';

export interface TuiConfig {
  baseUrl: string;
  bootstrapToken?: string;
  sessionToken?: string;
  currentProjectId?: string;
}

/** Tokens stay in memory. Redirects are forbidden so credentials cannot leak. */
export class DaemonClient {
  private token: string;
  private renewal: Promise<void> | undefined;
  private abort = new AbortController();
  private subscriptions = new Set<() => void>();
  readonly baseUrl: string;
  constructor(private config: TuiConfig, private fetcher: typeof fetch = fetch,
    private createSocket: WebSocketFactory = (url, protocols) => new WebSocket(url, protocols)) {
    const url = new URL(config.baseUrl);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash)
      throw new Error('Invalid daemon URL');
    if (url.protocol !== 'https:' && !['localhost', '127.0.0.1', '[::1]'].includes(url.hostname))
      throw new Error('Non-loopback daemon connections require HTTPS');
    this.baseUrl = url.href.replace(/\/$/, '').replace(/\/api$/, '') + '/api';
    this.token = config.sessionToken ?? '';
  }
  close(): void {
    if (this.abort.signal.aborted) return;
    this.abort.abort();
    for (const stop of this.subscriptions) stop();
    this.token = '';
  }
  /** Call after HTTP recent bootstrap. Keep polling for durable correctness. */
  subscribe(projectId: string, after: number, onEvent: (event: StreamEvent) => void,
    onState: (state: StreamState) => void = () => {}): () => void {
    this.abort.signal.throwIfAborted();
    const stop = subscribeEvents({
      baseUrl: this.baseUrl, projectId, after, onEvent, onState, createSocket: this.createSocket,
      getToken: async (refresh) => {
        this.abort.signal.throwIfAborted();
        // Native WebSocket hides handshake status. Refresh before every retry when
        // bootstrap credentials are available, not only after an observable 401.
        if (refresh && this.config.bootstrapToken) await this.renew();
        else await this.authenticate();
        this.abort.signal.throwIfAborted();
        return this.token;
      },
    });
    const cleanup = () => { this.subscriptions.delete(cleanup); stop(); };
    this.subscriptions.add(cleanup);
    return cleanup;
  }
  async authenticate(): Promise<void> {
    this.abort.signal.throwIfAborted();
    if (this.token) return;
    if (!this.config.bootstrapToken) throw new Error('A bootstrap token or session token is required');
    await this.renew();
  }
  private async renew(): Promise<void> {
    this.abort.signal.throwIfAborted();
    if (!this.config.bootstrapToken) throw new Error('Session expired. Reopen NOSH with a bootstrap token.');
    if (!this.renewal) this.renewal = (async () => {
      const response = await this.fetcher(this.baseUrl + '/session', {
        method: 'POST', redirect: 'error', signal: AbortSignal.any([this.abort.signal, AbortSignal.timeout(15000)]),
        headers: { authorization: `Bearer ${this.config.bootstrapToken}` },
      });
      if (!response.ok) throw new Error(`Authentication failed (${response.status})`);
      const data = await response.json() as { token?: string };
      if (typeof data.token !== 'string' || !data.token) throw new Error('Daemon returned no session token');
      this.abort.signal.throwIfAborted();
      this.token = data.token;
    })().finally(() => { this.renewal = undefined; });
    await this.renewal;
  }
  async request<T>(path: string, body?: unknown): Promise<T> {
    if (!path.startsWith('/') || path.startsWith('//')) throw new Error('Invalid API path');
    await this.authenticate();
    const encoded = body === undefined ? undefined : JSON.stringify(body);
    for (let attempt = 0; attempt < 2; attempt++) {
      const response = await this.fetcher(this.baseUrl + path, {
        method: body === undefined ? 'GET' : 'POST', redirect: 'error',
        signal: AbortSignal.any([this.abort.signal, AbortSignal.timeout(15000)]),
        headers: { authorization: `Bearer ${this.token}`, 'content-type': 'application/json' },
        ...(encoded === undefined ? {} : { body: encoded }),
      });
      // Retry only an explicit authentication rejection, with the exact same body/key.
      if (response.status === 401 && attempt === 0) { await this.renew(); continue; }
      const data = await response.json() as T & { error?: string };
      if (!response.ok) throw new Error(`${response.status}: ${data.error ?? 'Daemon request failed'}`);
      return data;
    }
    throw new Error('Authentication failed');
  }
}
