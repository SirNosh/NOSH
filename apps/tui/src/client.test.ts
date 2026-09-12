import { afterEach, describe, it, expect, vi } from 'vitest';
import { DaemonClient } from './client.js';
import type { EventSocket } from './stream.js';
afterEach(() => { vi.useRealTimers(); });
const json = (data: unknown, status = 200) => new Response(JSON.stringify(data), {status});
describe('authenticated daemon client', () => {
  it('requires credentials rather than relying on anonymous loopback', async () => {
    const fetcher = vi.fn(); const client = new DaemonClient({baseUrl:'http://127.0.0.1:4317'},fetcher);
    await expect(client.request('/projects')).rejects.toThrow('token'); expect(fetcher).not.toHaveBeenCalled();
  });
  it('exchanges bootstrap and renews 401 preserving the mutation body', async () => {
    const fetcher = vi.fn().mockResolvedValueOnce(json({token:'session1'})).mockResolvedValueOnce(json({error:'expired'},401)).mockResolvedValueOnce(json({token:'session2'})).mockResolvedValueOnce(json({ok:true}));
    const client = new DaemonClient({baseUrl:'http://127.0.0.1:4317/api',bootstrapToken:'bootstrap'},fetcher);
    await client.request('/chat',{projectId:'p',idempotencyKey:'stable'});
    expect(fetcher.mock.calls[0]![0]).toBe('http://127.0.0.1:4317/api/session');
    expect(fetcher.mock.calls[0]![1].headers.authorization).toBe('Bearer bootstrap');
    expect(fetcher.mock.calls[1]![1].body).toBe(fetcher.mock.calls[3]![1].body);
    expect(fetcher.mock.calls[3]![1].headers.authorization).toBe('Bearer session2');
    expect(fetcher.mock.calls[3]![1].redirect).toBe('error');
  });
  it('does not retry ambiguous mutation errors or stale versions', async () => {
    const fetcher = vi.fn().mockResolvedValue(json({error:'stale version'},409));
    const client = new DaemonClient({baseUrl:'http://localhost:4317',sessionToken:'s'},fetcher);
    await expect(client.request('/missions/m/transition',{})).rejects.toThrow('409: stale version'); expect(fetcher).toHaveBeenCalledTimes(1);
  });
  it('rejects credential-bearing URLs and plaintext remote hosts', () => {
    expect(() => new DaemonClient({baseUrl:'http://remote.invalid'})).toThrow('HTTPS');
    expect(() => new DaemonClient({baseUrl:'https://user:pass@remote.invalid'})).toThrow('Invalid');
  });
});


describe('daemon live subscriptions', () => {
  const socket = (): EventSocket => ({ onopen: null, onmessage: null, onclose: null, onerror: null, close: vi.fn() });
  const flush = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };
  const disconnect = (ws: EventSocket) => ws.onclose?.call(ws as WebSocket, new Event('close') as CloseEvent);

  it('uses the HTTP bootstrap cursor and session subprotocol without adding credential URLs', async () => {
    const fetcher = vi.fn().mockResolvedValue(json({events:[{sequence:91}]}));
    const ws = socket(), createSocket = vi.fn(() => ws), onEvent = vi.fn(), onState = vi.fn();
    const client = new DaemonClient({baseUrl:'http://localhost:4317',sessionToken:'private-session'},fetcher,createSocket);
    const page = await client.request<{events:{sequence:number}[]}>('/events?projectId=p&after=0&recent=true');
    const stop = client.subscribe('p',page.events[0]!.sequence,onEvent,onState); await flush();
    expect(fetcher).toHaveBeenCalledOnce();
    expect(createSocket).toHaveBeenCalledWith('ws://localhost:4317/api/events?projectId=p&after=91',['auth.private-session']);
    expect(fetcher.mock.calls[0]![1].headers.authorization).toBe('Bearer private-session');
    stop(); client.close(); expect(ws.close).toHaveBeenCalledOnce();
  });

  it('refreshes bootstrap authentication before reconnect, preserving the last delivered cursor', async () => {
    vi.useFakeTimers();
    const fetcher = vi.fn().mockResolvedValue(json({token:'renewed-session'}));
    const first = socket(), next = socket(), createSocket = vi.fn().mockReturnValueOnce(first).mockReturnValue(next);
    const client = new DaemonClient({baseUrl:'http://localhost:4317',sessionToken:'old-session',bootstrapToken:'bootstrap-secret'},fetcher,createSocket);
    client.subscribe('p',5,vi.fn()); await flush();
    first.onmessage?.call(first as WebSocket,new MessageEvent('message',{data:JSON.stringify({eventId:'evt_1',sequence:9,type:'agent.started',scope:{projectId:'p'},payload:{}})}));
    disconnect(first); await vi.advanceTimersByTimeAsync(250);
    expect(fetcher).toHaveBeenCalledOnce();
    expect(fetcher.mock.calls[0]![0]).toBe('http://localhost:4317/api/session');
    expect(fetcher.mock.calls[0]![1].headers.authorization).toBe('Bearer bootstrap-secret');
    expect(fetcher.mock.calls[0]![1].redirect).toBe('error');
    expect(createSocket.mock.calls[1]).toEqual(['ws://localhost:4317/api/events?projectId=p&after=9',['auth.renewed-session']]);
    client.close(); await vi.advanceTimersByTimeAsync(60_000); expect(createSocket).toHaveBeenCalledTimes(2);
  });

  it('shares an in-flight HTTP token renewal with a reconnect attempt', async () => {
    vi.useFakeTimers();
    let finishRenew!: (response: Response) => void;
    const renewal = new Promise<Response>(resolve => { finishRenew = resolve; });
    const fetcher = vi.fn().mockResolvedValueOnce(json({},401)).mockReturnValueOnce(renewal).mockResolvedValue(json({projects:[]}));
    const sockets: EventSocket[] = [], createSocket = vi.fn(() => { const ws = socket(); sockets.push(ws); return ws; });
    const client = new DaemonClient({baseUrl:'http://localhost:4317',sessionToken:'expired-session',bootstrapToken:'b'},fetcher,createSocket);
    client.subscribe('p',0,vi.fn()); await flush();
    const request = client.request('/projects'); await flush();
    expect(fetcher).toHaveBeenCalledTimes(2);
    disconnect(sockets[0]!); await vi.advanceTimersByTimeAsync(250);
    expect(fetcher).toHaveBeenCalledTimes(2);
    finishRenew(json({token:'fresh-session'})); await request; await flush();
    expect(createSocket.mock.calls[1]).toEqual(['ws://localhost:4317/api/events?projectId=p&after=0',['auth.fresh-session']]);
    expect(fetcher.mock.calls.filter(call => String(call[0]).endsWith('/session'))).toHaveLength(1);
    client.close();
  });

  it('aborts pending auth and all subscriptions on close and cannot resurrect credentials', async () => {
    let complete!: (response: Response) => void;
    const fetcher = vi.fn().mockReturnValue(new Promise<Response>(resolve => { complete = resolve; }));
    const createSocket = vi.fn(() => socket());
    const client = new DaemonClient({baseUrl:'http://localhost:4317',bootstrapToken:'b'},fetcher,createSocket);
    client.subscribe('p',0,vi.fn()); await flush();
    const signal = fetcher.mock.calls[0]![1].signal as AbortSignal;
    client.close(); expect(signal.aborted).toBe(true);
    // Even a fetch implementation that ignores abort cannot repopulate the token.
    complete(json({token:'late-session'})); await flush();
    expect(createSocket).not.toHaveBeenCalled();
    await expect(client.authenticate()).rejects.toThrow();
    await expect(client.request('/projects')).rejects.toThrow();
    expect(() => client.subscribe('p',0,vi.fn())).toThrow();
    expect(fetcher).toHaveBeenCalledOnce();
  });

  it('closes each active subscription exactly once and cancels pending retries', async () => {
    vi.useFakeTimers();
    const sockets: EventSocket[] = [], createSocket = vi.fn(() => { const ws = socket(); sockets.push(ws); return ws; });
    const client = new DaemonClient({baseUrl:'http://localhost:4317',sessionToken:'s'},vi.fn(),createSocket);
    const first = client.subscribe('p',0,vi.fn()), second = client.subscribe('q',0,vi.fn()); await flush();
    disconnect(sockets[0]!); client.close(); first(); second(); client.close();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(createSocket).toHaveBeenCalledTimes(2);
    expect(sockets.map(ws => vi.mocked(ws.close).mock.calls.length)).toEqual([1,1]);
  });

  it('does not accept invalid subscription projects or cursors', () => {
    const client = new DaemonClient({baseUrl:'http://localhost:4317',sessionToken:'s'},vi.fn(),vi.fn());
    for (const after of [-1,1.5,Infinity,NaN,Number.MAX_SAFE_INTEGER+1]) expect(() => client.subscribe('p',after,vi.fn())).toThrow('cursor');
    expect(() => client.subscribe('',0,vi.fn())).toThrow('project'); client.close();
  });
});
