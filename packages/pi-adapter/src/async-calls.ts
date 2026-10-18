/**
 * Asynchronous daemon tool calls for a Pi task session. A slow call (a supervised Job, a network read) settles
 * within a short grace period as an ordinary tool result; otherwise the model gets a placeholder at once and keeps
 * working, and the real result is delivered later as a message that wakes it. Results are only ever appended, so
 * the cached prompt prefix stays valid.
 */
const ASYNC_GRACE_MS = 2_000;
const ASYNC_HEARTBEAT_MS = 600_000;
/** Results landing within this window of each other wake the model once. */
const BATCH_WINDOW_MS = 50;

type AsyncOutcome = { settled: true; value: unknown } | { settled: false; placeholder: string };

export class AsyncToolCalls {
  private readonly pending = new Map<string, { label: string; startedAt: number }>();
  private readonly inbox: string[] = [];
  private waiters: Array<() => void> = [];
  private runs = 0;
  private programs = new AbortController();
  /** Set by the session owner: delivers a result into a live agent run; returns false when no run is live. */
  deliver: (text: string) => boolean = () => false;

  constructor(private readonly graceMs = ASYNC_GRACE_MS) {}

  /** True while a nosh_run (direct, or inside a nosh_code program) is testing the task worktree. */
  get worktreeBusy(): boolean { return this.runs > 0; }
  /** Fires when the session is aborted or stopped: running nosh_code programs stop. */
  get signal(): AbortSignal { return this.programs.signal; }
  get running(): string[] { return [...this.pending.entries()].map(([callId, call]) => `${call.label} (call ${callId}, ${Math.round((Date.now() - call.startedAt) / 1000)}s)`); }
  get idle(): boolean { return !this.pending.size && !this.inbox.length; }

  async start(callId: string, label: string, work: Promise<unknown>, format: (value: unknown) => string): Promise<AsyncOutcome> {
    const settled = work.then((value) => ({ settled: true as const, value }), (error: unknown) => ({ settled: true as const, value: { accepted: false, error: error instanceof Error ? error.message : String(error) } }));
    let timer: ReturnType<typeof setTimeout> | undefined;
    const graced = await Promise.race([settled, new Promise<null>((resolve) => { timer = setTimeout(() => resolve(null), this.graceMs); })]);
    clearTimeout(timer);
    if (graced) return graced;
    this.pending.set(callId, { label, startedAt: Date.now() });
    void settled.then(({ value }) => {
      this.pending.delete(callId);
      const text = `Async result of ${label} (call ${callId}):\n${format(value)}`;
      if (!this.deliver(text)) this.inbox.push(text);
      this.wake();
    });
    return { settled: false, placeholder: `${label} is still running in the background. Its result arrives in a later message that wakes you: continue with independent read-only work, or end your turn with no tool calls to wait for it. Do not give your final answer while it is running.` };
  }

  /** Marks a nosh_run as testing the worktree from the moment it starts until it settles, whatever its outcome. */
  trackRun<T>(work: Promise<T>): Promise<T> { this.runs += 1; return work.finally(() => { this.runs -= 1; }); }

  /** Re-queue messages that were steered into a run that ended before consuming them. */
  requeue(texts: readonly string[]): void { if (texts.length) { this.inbox.unshift(...texts); this.wake(); } }

  /**
   * The next message that should wake the model: every result that landed together, or a heartbeat when calls have
   * run for heartbeatMs without a result. Undefined when nothing is pending or queued.
   */
  async next(heartbeatMs = ASYNC_HEARTBEAT_MS): Promise<string | undefined> {
    if (this.idle) return undefined;
    if (!this.inbox.length) {
      const woke = await new Promise<boolean>((resolve) => { const timer = setTimeout(() => resolve(false), heartbeatMs); this.waiters.push(() => { clearTimeout(timer); resolve(true); }); });
      if (!woke && !this.inbox.length) return `Heartbeat: waited ${Math.round(heartbeatMs / 1000)} seconds. Still running: ${this.running.join("; ")}. They continue in the background; end your turn with no tool calls to keep waiting.`;
    }
    await new Promise((resolve) => setTimeout(resolve, BATCH_WINDOW_MS));
    return this.inbox.splice(0).join("\n\n");
  }

  /** Wakes a sleeping wake loop at once (abort/stop); the loop re-checks cancellation. */
  interrupt(): void { this.wake(); }

  /** Abort/stop: stops running nosh_code programs and wakes the loop; later programs get a fresh signal. */
  cancel(): void { this.programs.abort(); this.programs = new AbortController(); this.wake(); }

  private wake(): void { const waiters = this.waiters; this.waiters = []; for (const waiter of waiters) waiter(); }
}
