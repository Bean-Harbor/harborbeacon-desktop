//! Per-user session state for the HarborBeacon Desktop Agent.
//!
//! Each Feishu user gets a persistent JSON file under `base_dir/` that
//! remembers their current mode, active task, pending steps, and last
//! action/result so that "继续" and "重试" work across restarts.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

// ---- SessionMode -------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    /// Default: direct action execution.
    #[default]
    Coding,
    /// Planning: the agent records tasks/steps rather than executing them.
    Planning,
}

// ---- UserSession -------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserSession {
    /// Feishu sender_id — used as the file key.
    pub user_id: String,
    /// Current interaction mode.
    pub mode: SessionMode,
    /// Free-text description of the ongoing task.
    pub current_task: Option<String>,
    /// Ordered list of steps not yet executed (front = next to run).
    pub pending_steps: Vec<String>,
    /// Human-readable summary of the last action's output.
    pub last_result: Option<String>,
    /// Raw text of the last action so "重试" can re-dispatch it.
    pub last_action: Option<String>,
    /// Unix timestamp (seconds) of last update.
    pub updated_at: u64,
}

// ---- SessionError ------------------------------------------------------

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

// ---- SessionStore ------------------------------------------------------

pub struct SessionStore {
    base_dir: PathBuf,
}

impl SessionStore {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Build a sandboxed path for the given user_id.
    /// Replaces any character that is not alphanumeric, `-`, or `_` with `_`
    /// so the file stays inside `base_dir` regardless of what the id contains.
    fn path_for(&self, user_id: &str) -> PathBuf {
        let safe: String = user_id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.base_dir.join(format!("{safe}.json"))
    }

    /// Load session for `user_id`, returning a blank session on any error.
    pub fn load(&self, user_id: &str) -> UserSession {
        let path = self.path_for(user_id);
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(s) = serde_json::from_str::<UserSession>(&data) {
                return s;
            }
        }
        UserSession {
            user_id: user_id.to_string(),
            ..Default::default()
        }
    }

    /// Persist the session to disk. Creates `base_dir` if needed.
    pub fn save(&self, session: &UserSession) -> Result<(), SessionError> {
        fs::create_dir_all(&self.base_dir)?;
        let path = self.path_for(&session.user_id);
        let data = serde_json::to_string_pretty(session)?;
        fs::write(path, data)?;
        Ok(())
    }

    /// Remove the session file for `user_id` (no-op if it doesn't exist).
    pub fn clear(&self, user_id: &str) {
        let path = self.path_for(user_id);
        let _ = fs::remove_file(path);
    }
}

// ---- helpers -----------------------------------------------------------

/// Current Unix time in seconds.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---- Path for default sessions dir from workspace ----------------------

/// Returns `{workspace}/.harborbeacon/sessions`.
pub fn default_session_dir(workspace: &str) -> String {
    format!("{workspace}/.harborbeacon/sessions")
}
