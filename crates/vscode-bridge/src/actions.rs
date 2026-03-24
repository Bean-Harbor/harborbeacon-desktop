//! Safe workspace actions that stay inside the sandbox.

use crate::{ActionResult, BridgeBinding, BridgeError};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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

/// Show git diff for a workspace path (or "." for entire workspace).
pub fn git_diff(bridge: &BridgeBinding, rel_path: &str) -> Result<ActionResult, BridgeError> {
    let pathspec = normalize_pathspec(bridge, rel_path)?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&bridge.workspace.path)
        .arg("diff")
        .arg("--")
        .arg(pathspec)
        .output()
        .map_err(|e| BridgeError::Io(format!("git diff failed to start: {e}")))?;

    if !output.status.success() {
        return Ok(ActionResult {
            success: false,
            content: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    Ok(ActionResult {
        success: true,
        content: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

/// Apply a unified patch in the workspace using `git apply`.
pub fn apply_patch(bridge: &BridgeBinding, patch: &str) -> Result<ActionResult, BridgeError> {
    validate_patch_paths(patch)?;

    let mut check_cmd = Command::new("git")
        .arg("-C")
        .arg(&bridge.workspace.path)
        .arg("apply")
        .arg("--check")
        .arg("--whitespace=nowarn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| BridgeError::Io(format!("git apply --check failed to start: {e}")))?;

    if let Some(stdin) = check_cmd.stdin.as_mut() {
        stdin
            .write_all(patch.as_bytes())
            .map_err(|e| BridgeError::Io(format!("failed to write patch to git apply --check stdin: {e}")))?;
    }
    let check_output = check_cmd
        .wait_with_output()
        .map_err(|e| BridgeError::Io(format!("git apply --check failed: {e}")))?;

    if !check_output.status.success() {
        return Ok(ActionResult {
            success: false,
            content: String::from_utf8_lossy(&check_output.stderr).to_string(),
        });
    }

    let mut apply_cmd = Command::new("git")
        .arg("-C")
        .arg(&bridge.workspace.path)
        .arg("apply")
        .arg("--whitespace=nowarn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| BridgeError::Io(format!("git apply failed to start: {e}")))?;

    if let Some(stdin) = apply_cmd.stdin.as_mut() {
        stdin
            .write_all(patch.as_bytes())
            .map_err(|e| BridgeError::Io(format!("failed to write patch to git apply stdin: {e}")))?;
    }
    let apply_output = apply_cmd
        .wait_with_output()
        .map_err(|e| BridgeError::Io(format!("git apply failed: {e}")))?;

    if !apply_output.status.success() {
        return Ok(ActionResult {
            success: false,
            content: String::from_utf8_lossy(&apply_output.stderr).to_string(),
        });
    }

    Ok(ActionResult {
        success: true,
        content: "Patch applied successfully.".to_string(),
    })
}

/// Run workspace tests via Cargo.
pub fn run_tests(bridge: &BridgeBinding, filter: &str) -> Result<ActionResult, BridgeError> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&bridge.workspace.path)
        .arg("test")
        .arg("--workspace");
    if !filter.trim().is_empty() {
        cmd.arg(filter);
    }

    let output = cmd
        .output()
        .map_err(|e| BridgeError::Io(format!("cargo test failed to start: {e}")))?;

    let mut content = String::new();
    content.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    Ok(ActionResult {
        success: output.status.success(),
        content,
    })
}

/// Collect files up to `max_depth` levels deep.
fn collect_files(dir: &std::path::Path, max_depth: usize) -> Result<Vec<std::path::PathBuf>, BridgeError> {
    let mut out = Vec::new();
    collect_files_inner(dir, max_depth, &mut out)?;
    Ok(out)
}

fn normalize_pathspec(bridge: &BridgeBinding, rel_path: &str) -> Result<String, BridgeError> {
    let path = if rel_path.trim().is_empty() { "." } else { rel_path };
    if path == "." {
        return Ok(".".to_string());
    }

    let abs = bridge.resolve(path)?;
    let root = Path::new(&bridge.workspace.path)
        .canonicalize()
        .map_err(|e| BridgeError::Io(format!("workspace root not found: {e}")))?;
    let rel = abs
        .strip_prefix(&root)
        .map_err(|_| BridgeError::Denied("path is outside workspace".to_string()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

fn validate_patch_paths(patch: &str) -> Result<(), BridgeError> {
    for line in patch.lines() {
        if !(line.starts_with("--- ") || line.starts_with("+++ ")) {
            continue;
        }

        let path = line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| BridgeError::Denied("invalid patch header".to_string()))?;

        if path == "/dev/null" {
            continue;
        }

        let trimmed = path.trim_start_matches("a/").trim_start_matches("b/");
        if trimmed.contains("..") || trimmed.contains(':') || trimmed.starts_with('/') || trimmed.starts_with('\\') {
            return Err(BridgeError::Denied(format!(
                "patch path is not allowed in workspace sandbox: {trimmed}"
            )));
        }
    }
    Ok(())
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
