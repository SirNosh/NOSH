//! NOSH persistence: SQLite schema migrations, the per-project event store, and the host registry.
//!
//! Compatible with `packages/persistence` (better-sqlite3): the same schema and migrations, the
//! same canonical JSON in every stored column, and the same idempotency and terminal-submission
//! rules, so either implementation can open the other's database.

pub mod backup;
mod database;
mod event_store;
mod host_registry;
mod migrations;
pub mod restore;

pub use database::{SCHEMA_VERSION, open_database};
pub use event_store::{
    Appended, EventStore, GraphInput, Page, ProjectionInput, ProjectionMutation,
};
pub use host_registry::HostRegistry;

use nosh_wire::Issue;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// A record or draft failed wire validation.
    Invalid(Vec<Issue>),
    /// The store refused the operation; the message matches the TypeScript store's.
    Rejected(String),
    /// A restore committed, but cleaning up after it failed (`RestoreCommittedMaintenanceError`):
    /// the Project is restored and must not be rolled back or restored again.
    RestoreCommitted(String),
    /// A restore failed and undoing it hit errors too. Displays TypeScript's summary;
    /// `causes` holds the original error and each rollback failure.
    RollbackFailed {
        summary: String,
        causes: Vec<String>,
    },
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
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
            Error::Rejected(message) | Error::RestoreCommitted(message) => f.write_str(message),
            Error::RollbackFailed { summary, .. } => f.write_str(summary),
            Error::Sqlite(error) => error.fmt(f),
            Error::Json(error) => error.fmt(f),
            Error::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Error::Sqlite(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Json(error)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
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

impl From<Vec<Issue>> for Error {
    fn from(issues: Vec<Issue>) -> Self {
        Error::Invalid(issues)
    }
}

pub(crate) fn rejected<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Rejected(message.into()))
}
