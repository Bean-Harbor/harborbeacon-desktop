use core_contracts::{AutonomyLevel, WorkspaceTarget};

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub autonomy: AutonomyLevel,
    pub workspace: Option<WorkspaceTarget>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            autonomy: AutonomyLevel::Supervised,
            workspace: None,
        }
    }
}
