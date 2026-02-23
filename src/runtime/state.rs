use std::fmt;

use serde::{Deserialize, Serialize};

use crate::runtime::action::Action;
use crate::runtime::error::RuntimeError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ContainerState {
    Creating,
    Created,
    Running,
    Stopped,
    Paused,
}
impl fmt::Display for ContainerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContainerState::Creating => write!(f, "creating"),
            ContainerState::Created => write!(f, "created"),
            ContainerState::Running => write!(f, "running"),
            ContainerState::Stopped => write!(f, "stopped"),
            ContainerState::Paused => write!(f, "paused"),
        }
    }
}
impl ContainerState {
    pub fn apply(&self, action: Action) -> Result<Option<ContainerState>, RuntimeError> {
        use Action::*;
        use ContainerState::*;

        match (self, action.clone()) {
            (Creating, Create) => Ok(Some(Created)),
            (Created, Start) => Ok(Some(Running)),
            (Running, Pause) => Ok(Some(Paused)),
            (Paused, Resume) => Ok(Some(Running)),
            (Running | Paused, Kill) => Ok(Some(Stopped)),
            _ => Err(RuntimeError::InvalidAction {
                state: self.clone(),
                action,
            }),
        }
    }
}
