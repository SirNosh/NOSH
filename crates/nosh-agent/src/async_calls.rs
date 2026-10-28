//! Asynchronous daemon tool calls for a Pi task session, as `async-calls.ts`. A slow call (a
//! supervised Job, a network read) settles within a short grace period as an ordinary tool result;
//! otherwise the model gets a placeholder at once and keeps working, and the real result is
//! delivered later as a message that wakes it. Results are only ever appended, so the cached
//! prompt prefix stays valid.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

pub const ASYNC_GRACE: Duration = Duration::from_millis(2_000);
pub const ASYNC_HEARTBEAT: Duration = Duration::from_millis(600_000);
/// Results landing within this window of each other wake the model once.
const BATCH_WINDOW: Duration = Duration::from_millis(50);

/// `AbortController` and its `AbortSignal` in one: cloning shares the same signal.
#[derive(Clone, Default)]
pub struct AbortSignal(Arc<SignalState>);

#[derive(Default)]
struct SignalState {
    aborted: AtomicBool,
    notify: Notify,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn aborted(&self) -> bool {
        self.0.aborted.load(Ordering::SeqCst)
    }

    pub fn abort(&self) {
        self.0.aborted.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// Resolves once the signal is aborted.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Registered before the check, so an abort between the two is not missed.
            notified.as_mut().enable();
            if self.aborted() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AsyncOutcome {
    Settled(Value),
    Running { placeholder: String },
}

type Deliver = Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub struct AsyncToolCalls {
    grace: Duration,
    state: Mutex<State>,
    wake: Notify,
    runs: Arc<AtomicUsize>,
}

struct State {
    /// In start order, as a `Map` iterates.
    pending: Vec<(String, String, Instant)>,
    inbox: VecDeque<String>,
    programs: AbortSignal,
    deliver: Deliver,
}

impl Default for AsyncToolCalls {
    fn default() -> Self {
        Self::new(ASYNC_GRACE)
    }
}

impl AsyncToolCalls {
    pub fn new(grace: Duration) -> Self {
        Self {
            grace,
            state: Mutex::new(State {
                pending: Vec::new(),
                inbox: VecDeque::new(),
                programs: AbortSignal::new(),
                deliver: Arc::new(|_| false),
            }),
            wake: Notify::new(),
            runs: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Set by the session owner: delivers a result into a live agent run; returns false when no
    /// run is live.
    pub fn set_deliver(&self, deliver: impl Fn(&str) -> bool + Send + Sync + 'static) {
        self.state().deliver = Arc::new(deliver);
    }

    /// True while a nosh_run (direct, or inside a nosh_code program) is testing the task worktree.
    pub fn worktree_busy(&self) -> bool {
        self.runs.load(Ordering::SeqCst) > 0
    }

    /// Fires when the session is aborted or stopped: running nosh_code programs stop.
    pub fn signal(&self) -> AbortSignal {
        self.state().programs.clone()
    }

    pub fn running(&self) -> Vec<String> {
        running(&self.state())
    }

    pub fn idle(&self) -> bool {
        let state = self.state();
        state.pending.is_empty() && state.inbox.is_empty()
    }

    /// Runs `work` (spawned at once), returning its value if it settles within the grace period,
    /// else a placeholder; the value is then delivered later. A failed call settles as
    /// `{ accepted: false, error }`.
    pub async fn start(
        self: &Arc<Self>,
        call_id: &str,
        label: &str,
        work: impl Future<Output = Result<Value, String>> + Send + 'static,
        format: impl FnOnce(&Value) -> String + Send + 'static,
    ) -> AsyncOutcome {
        let mut settled = tokio::spawn(async move {
            work.await
                .unwrap_or_else(|error| json!({ "accepted": false, "error": error }))
        });
        if let Ok(joined) = tokio::time::timeout(self.grace, &mut settled).await {
            return AsyncOutcome::Settled(joined.unwrap_or_else(|error| failed_task(&error)));
        }
        self.state()
            .pending
            .push((call_id.to_owned(), label.to_owned(), Instant::now()));
        let (this, call_id, label_text) = (Arc::clone(self), call_id.to_owned(), label.to_owned());
        tokio::spawn(async move {
            let value = settled.await.unwrap_or_else(|error| failed_task(&error));
            let deliver = {
                let mut state = this.state();
                state.pending.retain(|(id, _, _)| *id != call_id);
                Arc::clone(&state.deliver)
            };
            let text = format!(
                "Async result of {label_text} (call {call_id}):\n{}",
                format(&value)
            );
            if !deliver(&text) {
                this.state().inbox.push_back(text);
            }
            this.wake.notify_waiters();
        });
        AsyncOutcome::Running {
            placeholder: format!(
                "{label} is still running in the background. Its result arrives in a later message that wakes you: continue with independent read-only work, or end your turn with no tool calls to wait for it. Do not give your final answer while it is running."
            ),
        }
    }

    /// Marks a nosh_run as testing the worktree from this call until `work` settles or is
    /// dropped, whatever its outcome.
    pub fn track_run<T>(
        &self,
        work: impl Future<Output = T> + Send + 'static,
    ) -> impl Future<Output = T> + Send + 'static {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let guard = RunGuard(Arc::clone(&self.runs));
        async move {
            let _guard = guard;
            work.await
        }
    }

    /// Re-queue messages that were steered into a run that ended before consuming them.
    pub fn requeue(&self, texts: &[String]) {
        if texts.is_empty() {
            return;
        }
        {
            let mut state = self.state();
            for text in texts.iter().rev() {
                state.inbox.push_front(text.clone());
            }
        }
        self.wake.notify_waiters();
    }

    /// The next message that should wake the model: every result that landed together, or a
    /// heartbeat when calls have run for `heartbeat` without a result. `None` when nothing is
    /// pending or queued; an empty string after an interrupt with nothing queued.
    pub async fn next(&self, heartbeat: Duration) -> Option<String> {
        if self.idle() {
            return None;
        }
        let notified = self.wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.state().inbox.is_empty() {
            let woke = tokio::time::timeout(heartbeat, notified).await.is_ok();
            let state = self.state();
            if !woke && state.inbox.is_empty() {
                return Some(format!(
                    "Heartbeat: waited {} seconds. Still running: {}. They continue in the background; end your turn with no tool calls to keep waiting.",
                    round_seconds(heartbeat),
                    running(&state).join("; ")
                ));
            }
        }
        tokio::time::sleep(BATCH_WINDOW).await;
        let texts: Vec<String> = self.state().inbox.drain(..).collect();
        Some(texts.join("\n\n"))
    }

    /// Wakes a sleeping wake loop at once (abort/stop); the loop re-checks cancellation.
    pub fn interrupt(&self) {
        self.wake.notify_waiters();
    }

    /// Abort/stop: stops running nosh_code programs and wakes the loop; later programs get a
    /// fresh signal.
    pub fn cancel(&self) {
        let previous = std::mem::take(&mut self.state().programs);
        previous.abort();
        self.wake.notify_waiters();
    }
}

struct RunGuard(Arc<AtomicUsize>);

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn running(state: &State) -> Vec<String> {
    state
        .pending
        .iter()
        .map(|(call_id, label, started)| {
            format!(
                "{label} (call {call_id}, {}s)",
                round_seconds(started.elapsed())
            )
        })
        .collect()
}

/// `Math.round(ms / 1000)`.
fn round_seconds(duration: Duration) -> u64 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let seconds = (duration.as_secs_f64()).round() as u64;
    seconds
}

/// A panicking call is a failed call, as a thrown error is in TypeScript.
fn failed_task(error: &tokio::task::JoinError) -> Value {
    json!({ "accepted": false, "error": error.to_string() })
}
