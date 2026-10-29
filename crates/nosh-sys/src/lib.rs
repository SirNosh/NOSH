//! NOSH's operating-system layer: Node-compatible paths, process identity and control, Git, and
//! the Job supervisor, compatible with `packages/git-workspaces` and `packages/jobs`.

pub mod fs;
pub mod git;
pub mod job_wrapper;
pub mod jobs;
pub mod paths;
pub mod process;

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The operation was refused; the message matches the TypeScript implementation's.
    Rejected(String),
    /// A Git command failed; the message is its trimmed stderr (as TypeScript reports it).
    Git(String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Rejected(message) | Error::Git(message) => f.write_str(message),
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

pub(crate) fn rejected<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Rejected(message.into()))
}
