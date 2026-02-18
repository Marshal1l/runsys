use serde::{Deserialize, Serialize};

use crate::runtime::action::Action;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ContainerState {
    Creating,
    Created,
    Running,
    Stopped,
    Paused,
}
#[derive(Debug)]
pub enum StateError {
    InvalidAction {
        state: ContainerState,
        action: Action,
    },
}

impl ContainerState {
    pub fn apply(&self, action: Action) -> Option<ContainerState> {
        use Action::*;
        use ContainerState::*;

        match (self, action) {
            (Creating, Create) => Some(Created),
            (Created, Start) => Some(Running),
            (Running, Pause) => Some(Paused),
            (Paused, Resume) => Some(Running),
            (Running | Paused, Kill) => Some(Stopped),
            _ => None,
        }
    }
}
