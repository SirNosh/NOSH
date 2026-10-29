//! Where `Send` callbacks meet the daemon's one thread. The Pi adapter's tool, terminal, and event
//! callbacks and the Job supervisor's watcher threads are `Send`; daemon state is `Rc`. A callback
//! runs at once when it arrives on the daemon's own thread (an event emitted from the adapter's
//! tasks, which share the daemon's runtime, keeps the order TypeScript's synchronous `appendDraft`
//! gives it), and is otherwise posted to the daemon's mailbox.

use super::NoshDaemon;
use futures_util::future::LocalBoxFuture;
use nosh_agent::async_calls::BoxFuture;
use nosh_agent::terminal::{TerminalContext, TerminalHost};
use nosh_agent::tools::AgentHost;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot};

/// Work for the daemon thread: run now, or return a future to spawn there.
pub type Job = Box<dyn FnOnce(&Rc<NoshDaemon>) -> Option<LocalBoxFuture<'static, ()>> + Send>;

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The daemons living on this thread, by bridge key.
    static DAEMONS: RefCell<HashMap<u64, Weak<NoshDaemon>>> = RefCell::new(HashMap::new());
}

/// The sending side, held by everything that calls into the daemon from elsewhere.
#[derive(Clone)]
pub struct Bridge {
    key: u64,
    sender: mpsc::UnboundedSender<Job>,
}

/// The receiving side, drained by the daemon once it exists.
pub struct Inbox {
    key: u64,
    receiver: mpsc::UnboundedReceiver<Job>,
}

/// A connected bridge and inbox.
pub fn bridge() -> (Bridge, Inbox) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
    (Bridge { key, sender }, Inbox { key, receiver })
}

impl Inbox {
    /// Registers `daemon` on this thread and drains its mailbox until the daemon is gone. Needs a
    /// `LocalSet`.
    pub(super) fn attach(self, daemon: &Rc<NoshDaemon>) {
        let Inbox { key, mut receiver } = self;
        DAEMONS.with(|daemons| daemons.borrow_mut().insert(key, Rc::downgrade(daemon)));
        let weak = Rc::downgrade(daemon);
        tokio::task::spawn_local(async move {
            while let Some(job) = receiver.recv().await {
                let Some(daemon) = weak.upgrade() else { break };
                if let Some(work) = job(&daemon) {
                    tokio::task::spawn_local(work);
                }
            }
        });
    }
}

pub(super) fn detach(key: u64) {
    let _ = DAEMONS.try_with(|daemons| daemons.borrow_mut().remove(&key));
}

impl Bridge {
    pub(super) fn key(&self) -> u64 {
        self.key
    }

    fn local(&self) -> Option<Rc<NoshDaemon>> {
        DAEMONS
            .try_with(|daemons| daemons.borrow().get(&self.key).and_then(Weak::upgrade))
            .ok()
            .flatten()
    }

    /// Runs `job` with the daemon: now when on its thread, else on its thread soon. Dropped when
    /// the daemon is gone.
    pub fn run(&self, job: impl FnOnce(&Rc<NoshDaemon>) + Send + 'static) {
        if let Some(daemon) = self.local() {
            job(&daemon);
            return;
        }
        let _ = self.sender.send(Box::new(move |daemon| {
            job(daemon);
            None
        }));
    }

    /// Runs async `work` on the daemon thread and returns its result to any thread.
    pub fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(Rc<NoshDaemon>) -> LocalBoxFuture<'static, Result<T, String>> + Send + 'static,
    ) -> BoxFuture<Result<T, String>> {
        let (reply, answer) = oneshot::channel();
        let posted = self.sender.send(Box::new(move |daemon| {
            let work = work(Rc::clone(daemon));
            Some(Box::pin(async move {
                let _ = reply.send(work.await);
            }))
        }));
        Box::pin(async move {
            posted.map_err(|_| "NOSH daemon is stopped".to_owned())?;
            answer
                .await
                .unwrap_or_else(|_| Err("NOSH daemon is stopped".to_owned()))
        })
    }

    /// The event sink for `PiAdapter::spawn`.
    pub fn emitter(&self) -> nosh_agent::pi::Emit {
        let bridge = self.clone();
        Arc::new(move |event: Value| {
            bridge.run(move |daemon| {
                // TypeScript's appendDraft throws into the adapter; an event refused here (a
                // Project in backup maintenance) is dropped instead.
                let _ = daemon.append_draft(&event);
            });
        })
    }

    /// The adapter's submission host.
    pub fn agent_host(&self) -> Arc<dyn AgentHost> {
        Arc::new(self.clone())
    }

    /// The adapter's terminal-turn host.
    pub fn terminal_host(&self) -> Arc<dyn TerminalHost> {
        Arc::new(self.clone())
    }
}

fn message(error: crate::Error) -> String {
    error.to_string()
}

impl AgentHost for Bridge {
    fn submit(
        &self,
        tool: &str,
        project_id: &str,
        attempt_key: &str,
        record: Value,
        agent_id: &str,
    ) -> BoxFuture<Result<Value, String>> {
        let (tool, project_id, attempt_key, agent_id) = (
            tool.to_owned(),
            project_id.to_owned(),
            attempt_key.to_owned(),
            agent_id.to_owned(),
        );
        self.call(move |daemon| {
            Box::pin(async move {
                let agent = (!agent_id.is_empty()).then_some(agent_id.as_str());
                daemon
                    .submit_tool(&tool, &project_id, &attempt_key, &record, agent)
                    .await
                    .map_err(message)
            })
        })
    }

    fn emit(&self, event: Value) {
        (self.emitter())(event);
    }
}

impl TerminalHost for Bridge {
    fn admit(&self, context: &TerminalContext) -> BoxFuture<Result<Option<Value>, String>> {
        let context = context.clone();
        self.call(move |daemon| {
            Box::pin(async move { daemon.admit_terminal(&context).await.map_err(message) })
        })
    }

    fn submit(
        &self,
        context: &TerminalContext,
        records: Vec<Value>,
    ) -> BoxFuture<Result<Value, String>> {
        let context = context.clone();
        self.call(move |daemon| {
            Box::pin(async move {
                daemon
                    .submit_terminal(&context, records, false)
                    .await
                    .map_err(message)
            })
        })
    }

    fn reject(
        &self,
        context: &TerminalContext,
        reason: String,
    ) -> BoxFuture<Result<Value, String>> {
        let context = context.clone();
        self.call(move |daemon| {
            Box::pin(async move {
                daemon
                    .reject_terminal(&context, &reason)
                    .await
                    .map_err(message)
            })
        })
    }
}
