# HarborBeacon Desktop

HarborBeacon Desktop is the standalone desktop-host project for remote assistant workflows on Windows and macOS.

Recommended standalone repository name: `harborbeacon-desktop`

This workspace currently provides:

- a Rust workspace skeleton for the desktop-host architecture
- a `doctor` CLI for Feishu credential and connectivity diagnostics
- foundational crates for future Feishu, runtime, and VS Code bridge work

## Layout

- `apps/desktop`: Desktop agent — Feishu WS → dispatch → vscode-bridge → reply
- `apps/doctor`: Feishu diagnostics CLI
- `apps/mcp-server`: stdio MCP server for VS Code / Copilot
- `crates/core-contracts`: shared types (Channel, InboundMessage, OutboundMessage, etc.)
- `crates/feishu-provider`: Feishu API, WebSocket long-connection, reply client
- `crates/router-runtime`: runtime configuration
- `crates/vscode-bridge`: sandboxed workspace actions (read_file, list_directory, search_text)

## Quick Start

### Doctor CLI

```powershell
cargo run -p harborbeacon-desktop-doctor -- --app-id <APP_ID> --app-secret <APP_SECRET>
```

Use `--json` for machine-readable output.

### Desktop Agent

```powershell
cargo run -p harborbeacon-desktop -- \
  --app-id <APP_ID> --app-secret <APP_SECRET> \
  --workspace .
```

The agent connects to Feishu via WebSocket, receives messages, dispatches
`/read`, `/ls`, `/search` commands against the workspace, and replies
back to Feishu automatically.

### VS Code / Copilot MCP Integration

1. Build the MCP server:

   ```powershell
   cargo build --release -p harborbeacon-desktop-mcp-server
   ```

2. Copy `.vscode/mcp.json` into your target workspace's `.vscode/` folder
   (or merge the `"mcp"` section into your existing VS Code settings).

3. Update the `"command"` path to point to the built binary, and
   `"--workspace"` to the project root you want to expose.

4. Restart VS Code. The MCP tools (`read_file`, `list_directory`,
   `search_text`) will appear in Copilot's tool list.

## Tests

```powershell
cargo test --workspace
```
