//! Safe workspace actions that stay inside the sandbox.

use crate::{ActionResult, BridgeBinding, BridgeError};
use std::fs;

/// Read a file inside the workspace. Path must be relative.
pub fn read_file(bridge: &BridgeBinding, rel_path: &str) -> Result<ActionResult, BridgeError> {
    let abs = bridge.resolve(rel_path)?;
    let content = fs::read_to_string(&abs)
        .map_err(|e| BridgeError::Io(format!("read failed: {e}")))?;
    Ok(ActionResult {
        success: true,
        content,
    })
}

/// List entries in a directory inside the workspace. Path must be relative.
pub fn list_directory(bridge: &BridgeBinding, rel_path: &str) -> Result<ActionResult, BridgeError> {
    let abs = bridge.resolve(rel_path)?;
    if !abs.is_dir() {
        return Err(BridgeError::Io("not a directory".into()));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&abs).map_err(|e| BridgeError::Io(e.to_string()))? {
        let entry = entry.map_err(|e| BridgeError::Io(e.to_string()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let suffix = if entry.path().is_dir() { "/" } else { "" };
        entries.push(format!("{name}{suffix}"));
    }
    entries.sort();
    Ok(ActionResult {
        success: true,
        content: entries.join("\n"),
    })
}

/// Search for a text pattern in files under a directory (non-recursive, simple substring match).
pub fn search_text(
    bridge: &BridgeBinding,
    rel_path: &str,
    query: &str,
) -> Result<ActionResult, BridgeError> {
    let abs = bridge.resolve(rel_path)?;
    let mut results = Vec::new();

    let walker = if abs.is_dir() {
        collect_files(&abs, 3)?
    } else {
        vec![abs]
    };

    for file_path in walker {
        if let Ok(content) = fs::read_to_string(&file_path) {
            for (i, line) in content.lines().enumerate() {
                if line.contains(query) {
                    let rel = file_path.display();
                    results.push(format!("{rel}:{}: {line}", i + 1));
                }
            }
        }
    }

    Ok(ActionResult {
        success: true,
        content: if results.is_empty() {
            "No matches found.".into()
        } else {
            results.join("\n")
        },
    })
}

/// Collect files up to `max_depth` levels deep.
fn collect_files(dir: &std::path::Path, max_depth: usize) -> Result<Vec<std::path::PathBuf>, BridgeError> {
    let mut out = Vec::new();
    collect_files_inner(dir, max_depth, &mut out)?;
    Ok(out)
}

fn collect_files_inner(
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<std::path::PathBuf>,
) -> Result<(), BridgeError> {
    if depth == 0 {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|e| BridgeError::Io(e.to_string()))? {
        let entry = entry.map_err(|e| BridgeError::Io(e.to_string()))?;
        let path = entry.path();
        if path.is_file() {
            out.push(path);
        } else if path.is_dir() {
            collect_files_inner(&path, depth - 1, out)?;
        }
    }
    Ok(())
}
