import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { MAX_EVENT_BYTES, subscribeEvents, type EventSocket, type StreamEvent } from './stream.js';

class FakeSocket implements EventSocket {
  onopen: WebSocket['onopen'] = null;
  onmessage: WebSocket['onmessage'] = null;
  onerror: WebSocket['onerror'] = null;
  onclose: WebSocket['onclose'] = null;
  close = vi.fn();
  open() { this.onopen?.call(this as unknown as WebSocket, new Event('open')); }
  message(data: unknown) { this.onmessage?.call(this as unknown as WebSocket, new MessageEvent('message', { data })); }
  end() { this.onclose?.call(this as unknown as WebSocket, new Event('close') as CloseEvent); }
  error() { this.onerror?.call(this as unknown as WebSocket, new Event('error')); }
}
const event = (eventId: string, sequence: number | null, extra: Partial<StreamEvent> = {}): StreamEvent => ({
  eventId, sequence, type: sequence === null ? 'agent.text_delta' : 'agent.started',
  payload: sequence === null ? { delta: 'hello' } : {}, scope: { projectId: 'prj_test', agentId: 'agt_test' },
  correlationId: 'task_test', retention: sequence === null ? 'ephemeral' : 'persistent', ...extra,
});
const stops: (() => void)[] = [];
const flush = async () => { await Promise.resolve(); await Promise.resolve(); };
function setup(after = 12) {
  const sockets: FakeSocket[] = [];
  const createSocket = vi.fn(() => { const socket = new FakeSocket(); sockets.push(socket); return socket; });
  const getToken = vi.fn().mockResolvedValue('secret-session');
  const onEvent = vi.fn(); const onState = vi.fn();
  const stop = subscribeEvents({ baseUrl: 'http://127.0.0.1:4317/api', projectId: 'prj_test', after, getToken, createSocket, onEvent, onState });
  stops.push(stop);
  return { sockets, createSocket, getToken, onEvent, onState, stop };
}
beforeEach(() => { vi.useFakeTimers(); });
afterEach(() => { stops.splice(0).forEach(stop => stop()); vi.useRealTimers(); });

describe('native live event transport', () => {
  it('resumes the bootstrap cursor with auth only in the WebSocket subprotocol', async () => {
    const s = setup();
    expect(s.createSocket).not.toHaveBeenCalled();
    await flush();
    expect(s.createSocket).toHaveBeenCalledWith('ws://127.0.0.1:4317/api/events?projectId=prj_test&after=12', ['auth.secret-session']);
    expect(s.createSocket.mock.calls[0]![0]).not.toContain('secret-session');
    expect(s.getToken).toHaveBeenCalledWith(false);
    s.sockets[0]!.open();
    expect(s.onState.mock.calls.map(call => call[0])).toEqual(['connecting', 'connected']);
  });

  it('delivers exact Pi text/tool deltas, scopes projects, and deduplicates without advancing null sequences', async () => {
    const s = setup(); await flush(); const socket = s.sockets[0]!; socket.open();
    const first = event('evt_a', null, { payload: { delta: 'hello ' } });
    const second = event('evt_b', null, { payload: { delta: '世界' } });
    const tool = event('evt_c', null, { type: 'agent.tool_update', payload: { toolCallId: 'call_1', toolName: 'read', partialResult: { content: [] } } });
    for (const e of [first, first, second, tool, event('evt_wrong', 500, { scope: { projectId: 'other' } }), event('evt_old', 12), event('evt_d', 16), event('evt_dup', 16), event('evt_outoforder', 15)]) socket.message(JSON.stringify(e));
    expect(s.onEvent.mock.calls.map(call => call[0])).toEqual([first, second, tool, event('evt_d', 16)]);
    socket.end();
    expect(s.onState).toHaveBeenLastCalledWith('disconnected');
    await vi.advanceTimersByTimeAsync(250);
    expect(s.getToken.mock.calls).toEqual([[false], [true]]);
    expect(s.createSocket.mock.calls[1]![0]).toContain('after=16');
  });

  it('acks only ingested events, so consumer errors replay from the previous durable cursor', async () => {
    const s = setup(); await flush(); s.onEvent.mockImplementationOnce(() => { throw new Error('consumer failed'); });
    s.sockets[0]!.message(JSON.stringify(event('evt_failed', 20)));
    expect(s.onState).toHaveBeenLastCalledWith('disconnected');
    await vi.advanceTimersByTimeAsync(250);
    expect(s.createSocket.mock.calls[1]![0]).toContain('after=12');
    s.sockets[1]!.message(JSON.stringify(event('evt_failed', 20)));
    expect(s.onEvent).toHaveBeenCalledTimes(2);
  });

  it('closes and detaches once; late socket callbacks cannot replay into a new subscription', async () => {
    const s = setup(); await flush(); const socket = s.sockets[0]!;
    const lateMessage = socket.onmessage!, lateClose = socket.onclose!, lateOpen = socket.onopen!;
    s.stop(); s.stop();
    lateMessage.call(socket as unknown as WebSocket, new MessageEvent('message', { data: JSON.stringify(event('evt_late', null)) }));
    lateClose.call(socket as unknown as WebSocket, new Event('close') as CloseEvent);
    lateOpen.call(socket as unknown as WebSocket, new Event('open'));
    await vi.advanceTimersByTimeAsync(60_000);
    expect(s.onEvent).not.toHaveBeenCalled(); expect(s.createSocket).toHaveBeenCalledTimes(1);
    expect(socket.close).toHaveBeenCalledTimes(1);
    expect([socket.onopen, socket.onclose, socket.onmessage, socket.onerror]).toEqual([null, null, null, null]);
    expect(s.onState.mock.calls.map(call => call[0])).toEqual(['connecting', 'disconnected']);
  });

  it('cleanup before admission or while authentication is pending cannot create a socket', async () => {
    const first = setup(); first.stop(); await flush(); expect(first.getToken).not.toHaveBeenCalled();
    const s = setup(); let resolve!: (token: string) => void;
    s.getToken.mockReturnValue(new Promise<string>(done => { resolve = done; }));
    await flush(); s.stop(); resolve('late-secret'); await flush();
    expect(s.createSocket).not.toHaveBeenCalled();
  });

  it('backs off repeated failures with a ten-second cap, including short-lived opens', async () => {
    const s = setup(); await flush();
    for (const delay of [250, 500, 1000, 2000, 4000, 8000, 10_000, 10_000]) {
      const count = s.sockets.length;
      s.sockets.at(-1)!.open(); s.sockets.at(-1)!.error();
      await vi.advanceTimersByTimeAsync(delay - 1); expect(s.sockets).toHaveLength(count);
      await vi.advanceTimersByTimeAsync(1); expect(s.sockets).toHaveLength(count + 1);
    }
    // A stable connection resets the delay, not every successful handshake.
    s.sockets.at(-1)!.open(); await vi.advanceTimersByTimeAsync(5000); s.sockets.at(-1)!.end();
    const count = s.sockets.length;
    await vi.advanceTimersByTimeAsync(250); expect(s.sockets).toHaveLength(count + 1);
  });

  it('times out a stalled handshake and cancels reconnect timers on cleanup', async () => {
    const s = setup(); await flush();
    await vi.advanceTimersByTimeAsync(15_000);
    expect(s.onState).toHaveBeenLastCalledWith('disconnected');
    expect(s.sockets[0]!.close).toHaveBeenCalledOnce();
    s.stop(); await vi.advanceTimersByTimeAsync(60_000);
    expect(s.createSocket).toHaveBeenCalledOnce();
  });

  it('retries authentication failures without exposing their error or token', async () => {
    const s = setup(); s.getToken.mockRejectedValueOnce(new Error('credential secret-session'));
    await flush(); expect(s.createSocket).not.toHaveBeenCalled();
    expect(s.onState).toHaveBeenLastCalledWith('disconnected');
    await vi.advanceTimersByTimeAsync(250);
    expect(s.getToken.mock.calls).toEqual([[false], [true]]);
    expect(JSON.stringify(s.onState.mock.calls)).not.toContain('secret-session');
    expect(s.createSocket).toHaveBeenCalledOnce();
  });

  it.each(['bad token', 'secret\nheader', '', 'x'.repeat(8193)])('rejects invalid protocol credentials safely (%#)', async token => {
    const s = setup(); s.getToken.mockResolvedValue(token); await flush();
    expect(s.createSocket).not.toHaveBeenCalled(); expect(s.onState).toHaveBeenLastCalledWith('disconnected');
  });

  it.each([
    ['invalid JSON', '{'],
    ['array envelope', '[]'],
    ['missing sequence', JSON.stringify({ eventId: 'evt_bad', type: 'agent.started', scope: { projectId: 'prj_test' }, payload: {} })],
    ['invalid sequence', JSON.stringify(event('evt_bad', -1))],
    ['fractional sequence', JSON.stringify(event('evt_bad', 1.5))],
    ['retention mismatch', JSON.stringify(event('evt_bad', 13, { retention: 'ephemeral' }))],
    ['oversized ASCII', 'x'.repeat(MAX_EVENT_BYTES + 1)],
    ['oversized UTF-8', '界'.repeat(Math.floor(MAX_EVENT_BYTES / 2))],
    ['binary frame', new ArrayBuffer(4)],
  ])('rejects %s frames, preserving the durable replay cursor', async (_name, data) => {
    const s = setup(); await flush(); s.sockets[0]!.message(data);
    expect(s.onEvent).not.toHaveBeenCalled(); expect(s.sockets[0]!.close).toHaveBeenCalledOnce();
    await vi.advanceTimersByTimeAsync(250);
    expect(s.createSocket.mock.calls[1]![0]).toContain('after=12');
  });

  it('accepts a bounded large text frame and never queues frames for rendering', async () => {
    const s = setup(); await flush();
    const bounded = event('evt_large', null, { payload: { delta: 'a'.repeat(MAX_EVENT_BYTES - 512) } });
    s.sockets[0]!.message(JSON.stringify(bounded));
    expect(s.onEvent).toHaveBeenCalledWith(bounded);
    for (let i = 0; i < 2000; i++) s.sockets[0]!.message(JSON.stringify(event(`evt_${i}`, 13 + i)));
    expect(s.onEvent).toHaveBeenCalledTimes(2001);
    s.sockets[0]!.end(); await vi.advanceTimersByTimeAsync(250);
    expect(s.createSocket.mock.calls[1]![0]).toContain('after=2012');
  });

  it('uses wss for HTTPS and URL-encodes project IDs without mixing them with credentials', async () => {
    const s = setup(); s.stop();
    stops.push(subscribeEvents({ baseUrl: 'https://example.test/api', projectId: 'project & after=0', after: 42, getToken: s.getToken,
      createSocket: s.createSocket, onEvent: s.onEvent, onState: s.onState }));
    await flush(); const url = new URL(s.createSocket.mock.calls[0]![0]);
    expect(url.protocol).toBe('wss:'); expect(url.searchParams.get('projectId')).toBe('project & after=0');
    expect(url.searchParams.get('after')).toBe('42'); expect([...url.searchParams.keys()]).toEqual(['projectId', 'after']);
  });
});
