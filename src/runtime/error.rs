use crate::runtime::{action::Action, state::ContainerState};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid action '{action:?}' for current state '{state:?}'")]
    InvalidAction {
        state: ContainerState,
        action: Action,
    },

    #[error("bundle path is not a directory: {0}")]
    InvalidBundle(PathBuf),

    #[error("invalid container state: {0}")]
    InvalidState(String),

    #[error("config.json not found in bundle: {0}")]
    ConfigNotFound(PathBuf),

    #[error("failed to parse OCI config: {0}")]
    ConfigParseError(#[source] serde_json::Error),

    #[error("container not found: {0}")]
    ContainerNotFound(String),

    #[error("container already exists: {0}")]
    ContainerAlreadyExists(String),

    #[error("IO error: {0}")]
    IoError(#[source] std::io::Error),

    #[error("serialization error: {0}")]
    SerializationError(#[source] serde_json::Error),

    #[error("deserialization error: {0}")]
    DeserializationError(#[source] serde_json::Error),

    #[error("state.json id mismatch: expected {expected}, got {got}")]
    IdMismatch { expected: String, got: String },

    #[error("nix error: {0}")]
    NixError(#[source] nix::Error),

    #[error("CStringError: {0}")]
    CStringError(std::ffi::NulError),
}

impl From<std::io::Error> for RuntimeError {
    fn from(err: std::io::Error) -> Self {
        RuntimeError::IoError(err)
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(err: serde_json::Error) -> Self {
        RuntimeError::SerializationError(err)
    }
}

impl From<nix::Error> for RuntimeError {
    fn from(err: nix::Error) -> Self {
        RuntimeError::NixError(err)
    }
}
impl From<std::ffi::NulError> for RuntimeError {
    fn from(e: std::ffi::NulError) -> Self {
        RuntimeError::CStringError(e)
    }
}
