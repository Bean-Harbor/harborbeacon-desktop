use core_contracts::WorkspaceTarget;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub mod actions;

#[derive(Debug, Clone)]
pub struct BridgeBinding {
    pub workspace: WorkspaceTarget,
}

impl BridgeBinding {
    pub fn new(path: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            workspace: WorkspaceTarget {
                path: path.into(),
                label: label.into(),
            },
        }
    }

    /// Resolve and validate that `rel` stays inside the workspace root.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, BridgeError> {
        let root = Path::new(&self.workspace.path).canonicalize().map_err(|e| {
            BridgeError::Io(format!("workspace root not found: {e}"))
        })?;
        let candidate = root.join(rel).canonicalize().map_err(|e| {
            BridgeError::Io(format!("path resolution failed: {e}"))
        })?;
        if !candidate.starts_with(&root) {
            return Err(BridgeError::PathEscape(rel.to_string()));
        }
        Ok(candidate)
    }
}

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("io error: {0}")]
    Io(String),
    #[error("path escapes workspace sandbox: {0}")]
    PathEscape(String),
    #[error("action denied: {0}")]
    Denied(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub success: bool,
    pub content: String,
}