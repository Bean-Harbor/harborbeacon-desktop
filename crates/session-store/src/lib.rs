//! Per-user session state for the HarborBeacon Desktop Agent.
//!
//! Each Feishu user gets a persistent JSON file under `base_dir/` that
//! remembers their current mode, active task, pending steps, and last
//! action/result so that "继续" and "重试" work across restarts.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshotMeta {
    pub id: String,
    pub label: Option<String>,
    pub updated_at: u64,
    pub current_task: Option<String>,
    pub pending_steps: usize,
}

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

    fn sanitize_key(key: &str) -> String {
        key.chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// Build a sandboxed path for the given user_id.
    /// Replaces any character that is not alphanumeric, `-`, or `_` with `_`
    /// so the file stays inside `base_dir` regardless of what the id contains.
    fn path_for(&self, user_id: &str) -> PathBuf {
        let safe = Self::sanitize_key(user_id);
        self.base_dir.join(format!("{safe}.json"))
    }

    fn history_dir_for(&self, user_id: &str) -> PathBuf {
        let safe = Self::sanitize_key(user_id);
        self.base_dir.join("_history").join(safe)
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

    /// Save a snapshot of the current session for later restore.
    pub fn save_snapshot(
        &self,
        session: &UserSession,
        label: Option<&str>,
    ) -> Result<String, SessionError> {
        let history_dir = self.history_dir_for(&session.user_id);
        fs::create_dir_all(&history_dir)?;

        let ts = now_secs();
        let label_slug = label
            .unwrap_or("")
            .trim()
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_string();

        let id = if label_slug.is_empty() {
            format!("{ts}")
        } else {
            format!("{ts}-{label_slug}")
        };

        let path = history_dir.join(format!("{id}.json"));
        let data = serde_json::to_string_pretty(session)?;
        fs::write(path, data)?;
        Ok(id)
    }

    /// List snapshots for a user, newest first.
    pub fn list_snapshots(&self, user_id: &str) -> Result<Vec<SessionSnapshotMeta>, SessionError> {
        let history_dir = self.history_dir_for(user_id);
        if !history_dir.exists() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        for entry in fs::read_dir(history_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let raw = fs::read_to_string(&path)?;
            let s: UserSession = serde_json::from_str(&raw)?;

            let label = id.split_once('-').map(|(_, rest)| rest.to_string());
            out.push(SessionSnapshotMeta {
                id,
                label,
                updated_at: s.updated_at,
                current_task: s.current_task,
                pending_steps: s.pending_steps.len(),
            });
        }

        out.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(out)
    }

    /// Load a specific snapshot and bind it to the current user id.
    pub fn load_snapshot(&self, user_id: &str, snapshot_id: &str) -> Result<UserSession, SessionError> {
        let history_dir = self.history_dir_for(user_id);
        let path = history_dir.join(format!("{}.json", Self::sanitize_key(snapshot_id)));
        let raw = fs::read_to_string(path)?;
        let mut s: UserSession = serde_json::from_str(&raw)?;
        s.user_id = user_id.to_string();
        Ok(s)
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
