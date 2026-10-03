//! Why a command failed, and the exit status that says so.

use std::path::PathBuf;
use std::process::ExitCode;

use cloud_api::client::ClientError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not logged in; run `ahocloud login`")]
    NotLoggedIn,
    #[error("{0}")]
    Api(#[from] ClientError),
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{0}")]
    Invalid(String),
    /// Packing, reading or drafting from the directory failed.
    #[error(transparent)]
    Workdir(#[from] cloud_workdir::WorkdirError),
    /// The gate refused the manifest; the refusals were printed.
    #[error("the gate refuses this manifest")]
    Refused,
    /// The terminal session failed: connecting, the socket, or the sandbox's `Error` frame.
    #[error("terminal: {0}")]
    Terminal(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
        let path = path.into();
        move |source| Error::Io { path, source }
    }

    pub fn exit_code(&self) -> ExitCode {
        match self {
            Error::Refused => ExitCode::from(1),
            _ => ExitCode::from(2),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
