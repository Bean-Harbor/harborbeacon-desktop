use core_contracts::WorkspaceTarget;

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
}