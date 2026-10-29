//! NOSH orchestration: the typed runtime that runs execution threads, skills, and programs
//! (`packages/orchestration-runtime`), and the daemon's supervisors.
//!
//! Everything here runs on the daemon's one thread, inside a `tokio::task::LocalSet`: state is
//! `Rc`/`RefCell`, and work that outlives a call (a background program step, a deferred session
//! stop) is `spawn_local`ed.

pub mod daemon;
pub mod research_map;
pub mod research_spawner;
pub mod runtime;
pub mod single_instance;
pub mod task_postflight;
pub mod task_worktree;

use futures_util::FutureExt;
use futures_util::future::LocalBoxFuture;
use nosh_agent::pi::{Inspection, ModelSelection, PiAdapter, TerminalStep};
use nosh_agent::policy::SessionOptions;
use nosh_agent::terminal::TerminalContext;
use nosh_wire::Issue;
use serde_json::Value;
use std::fmt;

/// An orchestration failure. It is `Clone` because one execution's result is shared by every
/// caller awaiting it, as a TypeScript promise is.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// A record failed wire validation (a zod `parse` in TypeScript).
    Invalid(Vec<Issue>),
    /// The operation failed; the message matches the TypeScript implementation's.
    Rejected(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(issues) => {
                let details: Vec<String> = issues
                    .iter()
                    .map(|i| format!("{} {}: {}", i.pointer, i.code, i.message))
                    .collect();
                write!(f, "invalid record: {}", details.join("; "))
            }
            Error::Rejected(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

impl From<Vec<Issue>> for Error {
    fn from(issues: Vec<Issue>) -> Self {
        Error::Invalid(issues)
    }
}

impl From<nosh_store::Error> for Error {
    fn from(error: nosh_store::Error) -> Self {
        match error {
            nosh_store::Error::Invalid(issues) => Error::Invalid(issues),
            other => Error::Rejected(other.to_string()),
        }
    }
}

impl From<nosh_research::Error> for Error {
    fn from(error: nosh_research::Error) -> Self {
        match error {
            nosh_research::Error::Invalid(issues) => Error::Invalid(issues),
            other => Error::Rejected(other.to_string()),
        }
    }
}

impl From<nosh_sys::Error> for Error {
    fn from(error: nosh_sys::Error) -> Self {
        Error::Rejected(error.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Rejected(error.to_string())
    }
}

pub(crate) fn rejected<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Rejected(message.into()))
}

/// The Pi sessions orchestration drives: [`PiAdapter`] in the daemon, fakes in tests.
pub trait Agents {
    fn start(
        &self,
        options: SessionOptions,
    ) -> LocalBoxFuture<'_, std::result::Result<Inspection, String>>;
    fn prompt<'a>(
        &'a self,
        agent_id: &'a str,
        prompt: &'a str,
        step: Option<&'a TerminalStep>,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>>;
    fn steer<'a>(
        &'a self,
        agent_id: &'a str,
        message: &'a str,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>>;
    fn abort<'a>(
        &'a self,
        agent_id: &'a str,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>>;
    fn stop(&self, agent_id: &str);
    fn inspect(&self) -> Vec<Inspection>;
    /// Queues a prompt for after the running turn. Fakes that never run turns need not answer.
    fn follow_up<'a>(
        &'a self,
        _agent_id: &'a str,
        _message: &'a str,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Err("follow-up is not supported by these sessions".to_owned()) })
    }
    fn set_thinking_level<'a>(
        &'a self,
        _agent_id: &'a str,
        _level: &'a str,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Err("thinking levels are not supported by these sessions".to_owned()) })
    }
    /// Refuses a model selection Pi cannot resolve; no selection is always valid.
    fn validate_model_selection<'a>(
        &'a self,
        _selection: Option<&'a Value>,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// The model of every new session without an explicit selection.
    fn set_default_model(&self, _selection: Option<ModelSelection>) {}
    /// Whether the session's terminal turn may still record its outcome.
    fn terminal_turn_active(&self, _context: &TerminalContext) -> bool {
        false
    }
}

impl Agents for PiAdapter {
    fn start(
        &self,
        options: SessionOptions,
    ) -> LocalBoxFuture<'_, std::result::Result<Inspection, String>> {
        PiAdapter::start(self, options).boxed_local()
    }
    fn prompt<'a>(
        &'a self,
        agent_id: &'a str,
        prompt: &'a str,
        step: Option<&'a TerminalStep>,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>> {
        PiAdapter::prompt(self, agent_id, prompt, step).boxed_local()
    }
    fn steer<'a>(
        &'a self,
        agent_id: &'a str,
        message: &'a str,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>> {
        PiAdapter::steer(self, agent_id, message).boxed_local()
    }
    fn abort<'a>(
        &'a self,
        agent_id: &'a str,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>> {
        PiAdapter::abort(self, agent_id).boxed_local()
    }
    fn stop(&self, agent_id: &str) {
        PiAdapter::stop(self, agent_id);
    }
    fn inspect(&self) -> Vec<Inspection> {
        PiAdapter::inspect(self)
    }
    fn follow_up<'a>(
        &'a self,
        agent_id: &'a str,
        message: &'a str,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>> {
        PiAdapter::follow_up(self, agent_id, message).boxed_local()
    }
    fn set_thinking_level<'a>(
        &'a self,
        agent_id: &'a str,
        level: &'a str,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>> {
        PiAdapter::set_thinking_level(self, agent_id, level)
            .map(|result| result.map(drop))
            .boxed_local()
    }
    fn validate_model_selection<'a>(
        &'a self,
        selection: Option<&'a Value>,
    ) -> LocalBoxFuture<'a, std::result::Result<(), String>> {
        PiAdapter::validate_model_selection(self, selection).boxed_local()
    }
    fn set_default_model(&self, selection: Option<ModelSelection>) {
        PiAdapter::set_default_model(self, selection);
    }
    fn terminal_turn_active(&self, context: &TerminalContext) -> bool {
        PiAdapter::terminal_turn_active(self, context)
    }
}
