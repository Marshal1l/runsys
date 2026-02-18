use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::runtime::{
    action::Action,
    state::{ContainerState, StateError},
};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub bundle: PathBuf,
    pub state: ContainerState,
    pub pid: Option<u32>,
}
impl Container {
    pub fn apply_action(&mut self, action: Action) -> Result<(), StateError> {
        match self.state.apply(action.clone()) {
            Some(next) => {
                self.state = next;
                Ok(())
            }
            None => Err(StateError::InvalidAction {
                state: self.state.clone(),
                action,
            }),
        }
    }
}
