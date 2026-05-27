use std::{io, path::PathBuf};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid workspace path: {0}")]
    InvalidWorkspace(PathBuf),

    #[error("workspace not found from {0}")]
    WorkspaceNotFound(PathBuf),

    #[error("invalid entry name: {0}")]
    InvalidEntryName(String),

    #[error("invalid portal path: {0}")]
    InvalidPortalPath(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("target not found: {0}")]
    TargetNotFound(PathBuf),

    #[error("target is not a directory: {0}")]
    TargetNotDirectory(PathBuf),

    #[error("entry already exists: {0}")]
    EntryExists(String),

    #[error("entry not found: {0}")]
    EntryNotFound(String),

    #[error("daemon not running for workspace: {0}")]
    DaemonNotRunning(PathBuf),

    #[error("daemon already running for workspace: {0}")]
    DaemonAlreadyRunning(PathBuf),

    #[error("workspace state is corrupt: {0}")]
    StateCorrupt(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("cli error: {0}")]
    Cli(String),

    #[error("unsupported in scaffold: {0}")]
    Unsupported(&'static str),
}
