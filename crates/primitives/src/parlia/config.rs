use serde::{Deserialize, Serialize};

/// Configuration for the parlia consensus
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParliaConfig {
    /// The epoch number
    pub epoch: u64,
    /// The period of block proposal
    pub period: u64,
}

impl Default for ParliaConfig {
    fn default() -> Self {
        Self { epoch: 200, period: 3 }
    }
}
