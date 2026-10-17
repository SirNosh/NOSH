//! NOSH's research domain, compatible with `packages/evidence`, `graph`, `scheduler`, `skills`,
//! `episodes`, `threads`, and `agent-runtime`.

pub mod agent_runtime;
pub mod artifacts;
pub mod episodes;
pub mod graph;
pub mod scheduler;
pub mod skills;
pub mod threads;

use nosh_wire::Issue;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// A record failed wire validation (a zod `parse` in TypeScript).
    Invalid(Vec<Issue>),
    /// The operation was refused; the message matches the TypeScript implementation's.
    Rejected(String),
    Io(std::io::Error),
    Json(serde_json::Error),
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
            Error::Io(error) => error.fmt(f),
            Error::Json(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Json(error)
    }
}

impl From<Vec<Issue>> for Error {
    fn from(issues: Vec<Issue>) -> Self {
        Error::Invalid(issues)
    }
}

impl From<nosh_sys::Error> for Error {
    fn from(error: nosh_sys::Error) -> Self {
        match error {
            nosh_sys::Error::Io(error) => Error::Io(error),
            nosh_sys::Error::Json(error) => Error::Json(error),
            other => Error::Rejected(other.to_string()),
        }
    }
}

pub(crate) fn rejected<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Rejected(message.into()))
}
