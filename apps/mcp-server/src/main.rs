//! HarborBeacon Desktop MCP Server — stdio JSON-RPC transport.
//!
//! Speaks the Model Context Protocol over stdin/stdout so that
//! VS Code / Copilot (or any MCP client) can call workspace tools.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout.

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use vscode_bridge::{actions, BridgeBinding};

#[derive(Parser)]
#[command(name = "harborbeacon-mcp-server", about = "MCP server for HarborBeacon Desktop")]
struct Cli {
    /// Workspace root path to expose to MCP clients.
    #[arg(long)]
    workspace: String,
}

// ---------------------------------------------------------------------------
// JSON-RPC types (minimal subset needed for MCP)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn tool_list() -> Value {
    json!({
        "tools": [
            {
                "name": "read_file",
                "description": "Read a file inside the workspace (relative path).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the file." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "list_directory",
                "description": "List entries in a workspace directory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the directory." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "search_text",
                "description": "Search for a text pattern in workspace files.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative directory to search." },
                        "query": { "type": "string", "description": "Substring to find." }
                    },
                    "required": ["path", "query"]
                }
            }
        ]
    })
}

fn handle_tool_call(bridge: &BridgeBinding, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match name {
        "read_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            actions::read_file(bridge, path)
        }
        "list_directory" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            actions::list_directory(bridge, path)
        }
        "search_text" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            actions::search_text(bridge, path, query)
        }
        _ => {
            return json!({
                "content": [{ "type": "text", "text": format!("unknown tool: {name}") }],
                "isError": true
            });
        }
    };

    match result {
        Ok(r) => json!({
            "content": [{ "type": "text", "text": r.content }],
            "isError": false
        }),
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("error: {e}") }],
            "isError": true
        }),
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    let bridge = BridgeBinding::new(&cli.workspace, "mcp-workspace");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: RpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = RpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(RpcError {
                        code: -32700,
                        message: format!("parse error: {e}"),
                    }),
                };
                let _ = writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap());
                let _ = stdout.flush();
                continue;
            }
        };

        let id = req.id.clone().unwrap_or(Value::Null);

        let resp = match req.method.as_str() {
            "initialize" => RpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "harborbeacon-desktop-mcp-server",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
                error: None,
            },
            "tools/list" => RpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(tool_list()),
                error: None,
            },
            "tools/call" => RpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(handle_tool_call(&bridge, &req.params)),
                error: None,
            },
            _ => RpcResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(RpcError {
                    code: -32601,
                    message: format!("method not found: {}", req.method),
                }),
            },
        };

        let _ = writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap());
        let _ = stdout.flush();
    }
}
