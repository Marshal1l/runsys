use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    Create,
    Start,
    Pause,
    Resume,
    Kill,
}
