//! NOSH orchestration: the typed runtime that runs execution threads, skills, and programs
//! (`packages/orchestration-runtime`), and the daemon's supervisors.
//!
//! Everything here runs on the daemon's one thread, inside a `tokio::task::LocalSet`: state is
//! `Rc`/`RefCell`, and work that outlives a call (a background program step, a deferred session
//! stop) is `spawn_local`ed.

pub mod runtime;

use futures_util::FutureExt;
use futures_util::future::LocalBoxFuture;
use nosh_agent::pi::{Inspection, PiAdapter, TerminalStep};
use nosh_agent::policy::SessionOptions;
use nosh_wire::Issue;
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
}
