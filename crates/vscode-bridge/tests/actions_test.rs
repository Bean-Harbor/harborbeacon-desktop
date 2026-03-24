//! Integration tests for vscode-bridge safe workspace actions.

use std::fs;
use tempfile::TempDir;
use vscode_bridge::{actions, BridgeBinding};

fn setup() -> (TempDir, BridgeBinding) {
    let tmp = TempDir::new().expect("tempdir");
    // Create test file structure:
    //   hello.txt
    //   src/
    //     main.rs
    //   empty_dir/
    let root = tmp.path();
    fs::write(root.join("hello.txt"), "hello world\nline two\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src").join("main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    fs::create_dir_all(root.join("empty_dir")).unwrap();

    let bridge = BridgeBinding::new(
        root.to_str().unwrap().to_string(),
        "test-workspace".to_string(),
    );
    (tmp, bridge)
}

// ---- read_file ----

#[test]
fn read_file_ok() {
    let (_tmp, bridge) = setup();
    let result = actions::read_file(&bridge, "hello.txt").unwrap();
    assert!(result.success);
    assert!(result.content.contains("hello world"));
}

#[test]
fn read_file_nested() {
    let (_tmp, bridge) = setup();
    let result = actions::read_file(&bridge, "src/main.rs").unwrap();
    assert!(result.success);
    assert!(result.content.contains("fn main()"));
}

#[test]
fn read_file_not_found() {
    let (_tmp, bridge) = setup();
    let result = actions::read_file(&bridge, "nope.txt");
    assert!(result.is_err());
}

// ---- list_directory ----

#[test]
fn list_directory_root() {
    let (_tmp, bridge) = setup();
    let result = actions::list_directory(&bridge, ".").unwrap();
    assert!(result.success);
    assert!(result.content.contains("hello.txt"));
    assert!(result.content.contains("src/"));
    assert!(result.content.contains("empty_dir/"));
}

#[test]
fn list_directory_subdir() {
    let (_tmp, bridge) = setup();
    let result = actions::list_directory(&bridge, "src").unwrap();
    assert!(result.success);
    assert!(result.content.contains("main.rs"));
}

#[test]
fn list_directory_empty() {
    let (_tmp, bridge) = setup();
    let result = actions::list_directory(&bridge, "empty_dir").unwrap();
    assert!(result.success);
    assert!(result.content.is_empty());
}

#[test]
fn list_directory_on_file_fails() {
    let (_tmp, bridge) = setup();
    let result = actions::list_directory(&bridge, "hello.txt");
    assert!(result.is_err());
}

// ---- search_text ----

#[test]
fn search_text_found() {
    let (_tmp, bridge) = setup();
    let result = actions::search_text(&bridge, ".", "hello").unwrap();
    assert!(result.success);
    assert!(result.content.contains("hello world"));
}

#[test]
fn search_text_not_found() {
    let (_tmp, bridge) = setup();
    let result = actions::search_text(&bridge, ".", "zzz_nonexistent_zzz").unwrap();
    assert!(result.success);
    assert!(result.content.contains("No matches"));
}

#[test]
fn search_text_in_subdir() {
    let (_tmp, bridge) = setup();
    let result = actions::search_text(&bridge, "src", "println").unwrap();
    assert!(result.success);
    assert!(result.content.contains("println"));
}

// ---- sandbox escape ----

#[test]
fn sandbox_escape_blocked() {
    let (_tmp, bridge) = setup();
    let result = actions::read_file(&bridge, "../../etc/passwd");
    assert!(result.is_err());
}
