//! Git remote information.

use serde::{Deserialize, Serialize};

/// Remote repository information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInfo {
    /// Name of the remote (e.g., "origin", "upstream").
    pub name: String,
    /// URI of the remote repository.
    pub uri: String,
    /// Detected main branch name for this remote.
    pub main_branch: String,
}
